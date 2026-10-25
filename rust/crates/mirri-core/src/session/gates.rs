//! Human-in-the-loop gates: tool approval and user questions.
//!
//! The engine never blocks the loop on an unknown decision; instead it
//! registers a pending request with the gate, publishes a [`SessionEvent`]
//! on the [`EventBus`] so external listeners (TUI, API handler) learn the
//! `request_id`, and awaits the decision:
//!
//! ```text
//! engine ── request() ──► ApprovalGate ──► (ToolApprovalRequested) ──► listener
//!    ▲                                                                      │
//!    └────────── ApprovalDecision ◄─────────── resolve(request_id) ◄────────┘
//! ```
//!
//! Both gates are session-scoped artifacts: every session owns its own gate
//! instances, bundled with its [`EventBus`] in
//! [`SessionArtifacts`](crate::session::artifacts::SessionArtifacts) and
//! installed by
//! [`SessionManager`](crate::session::manager::SessionManager) on create or
//! resume. When a session closes,
//! [`SessionShutdown`](crate::session::shutdown::SessionShutdown) drains that
//! session's gates, so no awaiting future is left hanging — and no other
//! session's requests are affected.
//!
//! ## Pending snapshots (B4-a)
//!
//! Every pending entry carries the request's payload **snapshot**
//! ([`ApprovalSnapshot`] / [`QuestionSnapshot`]): tool name, display, the
//! structured question items, a millisecond `created_at`, and the optional
//! agent/turn/tool-call attribution. The snapshot makes the gate the single
//! pending source of truth — `approval.listPending`, `question.listPending`,
//! and the interaction kernel all read it directly. Resolved ids are recorded
//! in each gate's [`ResolvedRing`], the bounded memory behind
//! `interaction.isRecentlyResolved`.
//!
//! ## Concurrency
//!
//! The pending maps are guarded by a `std::sync::Mutex` and never held
//! across an `.await`: each method locks only to insert or remove a
//! `oneshot::Sender`, then releases the lock before awaiting the
//! corresponding receiver.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::events::{EventBus, SessionEvent};

/// Current wall-clock time in milliseconds since the Unix epoch (the
/// `createdAt` spelling the klient interaction contract carries). Public so
/// the turn-flow ask seams stamp their snapshots with the same clock.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Bounded ring of recently-resolved request ids.
///
/// Backs `interaction.isRecentlyResolved`: every successful
/// resolve/answer/dismiss records the request id here, and the ring forgets
/// the oldest entry once it holds [`ResolvedRing::CAPACITY`] ids — a
/// session-long "recently" window with a hard memory bound.
///
/// The ring ALSO remembers the verbatim texts of recently DISMISSED
/// questions (ADR-007 re-ask guard: `dismiss()` feeds the pending
/// snapshot's question texts, `AskUserQuestion` rejects an exact re-ask).
/// Only dismissal records texts — answering never feeds this side of the
/// ring, so re-asking an answered question stays allowed (SDR-005 (d)).
#[derive(Debug, Default)]
pub struct ResolvedRing {
    ids: Mutex<VecDeque<Uuid>>,
    dismissed_texts: Mutex<VecDeque<String>>,
}

impl ResolvedRing {
    /// How many resolved ids the ring remembers before it starts forgetting
    /// the oldest.
    const CAPACITY: usize = 256;

    /// Record a resolved request id, evicting the oldest entry past the
    /// capacity.
    pub fn record(&self, id: Uuid) {
        let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
        if ids.len() >= Self::CAPACITY {
            ids.pop_front();
        }
        ids.push_back(id);
    }

    /// Whether the ring still remembers the given request id.
    pub fn contains(&self, id: Uuid) -> bool {
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&id)
    }

    /// Record the verbatim text of a dismissed question, evicting the
    /// oldest entry past the capacity.
    pub fn record_dismissed_text(&self, text: String) {
        let mut texts = self
            .dismissed_texts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if texts.len() >= Self::CAPACITY {
            texts.pop_front();
        }
        texts.push_back(text);
    }

    /// Whether the ring still remembers the given question text as
    /// dismissed.
    pub fn contains_dismissed_text(&self, text: &str) -> bool {
        self.dismissed_texts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|dismissed| dismissed == text)
    }
}

/// Outcome of a tool-approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The tool call may proceed (one-off — nothing is memorized).
    Approved,
    /// The tool call may proceed AND the decision is memorized as a
    /// session-scoped approval rule (the gate-channel counterpart of the
    /// reverse-RPC reply `{ "decision": "approved", "scope": "session" }`).
    ApprovedForSession,
    /// The tool call must not proceed; `reason` explains why.
    Denied(String),
    /// No decision arrived in time (session closed via drain, or no
    /// responder posted one before the requester gave up).
    Timeout,
}

/// The payload snapshot of one pending approval request — everything
/// `approval.listPending` (and the interaction kernel's approval rows)
/// needs to present the request, captured at registration time.
#[derive(Debug, Clone)]
pub struct ApprovalSnapshot {
    /// The tool whose execution needs approval.
    pub tool_name: String,
    /// The action being approved (e.g. `"execute Bash"`).
    pub action: String,
    /// The tool input's display value (the protocol `ToolInputDisplay`
    /// union; crosses the wire uninspected).
    pub display: Value,
    /// The requesting agent's uuid, or `Uuid::nil()` when unknown (the main
    /// agent's flow carries no identity). A subagent flow's id is the uuid
    /// part of its `agent-<uuid>` string id — round-trippable back to the
    /// full id by consumers that know the session roster (supervision B1).
    pub agent_id: Uuid,
    /// The engine turn that asked, when known.
    pub turn_id: Option<u64>,
    /// The tool call the approval gates, when known.
    pub tool_call_id: Option<String>,
    /// Milliseconds since the Unix epoch when the request registered.
    pub created_at: u64,
    /// The engine approval-rule facet the call was gated on (e.g. `"Bash"`),
    /// when the tool reported one — the pattern a "always allow" upgrade or
    /// a session-scoped approval memorizes. `None` = the ask carries no
    /// facet (the klient field is optional and omitted).
    pub approval_rule: Option<String>,
    /// The directory this ask offers for once/session/persist granting
    /// (plan D3): the `WorkspaceBoundaryPolicy` names the path a user may
    /// grant into the sandbox. `None` = the ask is a plain tool approval
    /// with no directory grant attached.
    pub grant_dir: Option<PathBuf>,
}

/// Applies an approved directory grant into a session's sandbox (plan D3).
///
/// Defined in the core so the ask/reply/decide surfaces share one contract;
/// implementations live at the composition root (mirri-embedded applies the
/// grant to the session's shared [`WorkspacePaths`](crate::tools::WorkspacePaths)
/// and — for `persist` — writes the workspace-local config so a later
/// session on the same workspace restores it). Zero-IO: the core never
/// touches the filesystem here — a `None` sink simply degrades a
/// session/persist decision to a one-off approval.
pub trait AdditionalDirGrantSink: Send + Sync {
    /// Grant `dir` into the session sandbox; `persist` additionally writes
    /// the dir into the workspace-local config (`<projectRoot>/.mirri-code/
    /// local.toml`'s `workspace.additional_dir`) so a later session on the
    /// same workspace reseeds it.
    fn apply_grant(&self, dir: &Path, persist: bool);
}

/// One pending approval entry: the awaiting requester's oneshot sender
/// plus the request's payload snapshot. Not `Debug` — it holds a
/// decision-side callback.
struct PendingApproval {
    sender: oneshot::Sender<ApprovalDecision>,
    snapshot: ApprovalSnapshot,
    /// Decision-side wake (ADR-011, swarm T4): invoked by [`resolve`]
    /// itself — the `approval.decide` RPC handler, independent of the
    /// asker's turn polling — so a member parked by the batch scheduler
    /// (whose turn is held unpolled) still learns the decision. The
    /// turn-side resolution notification only fires when the flow is next
    /// polled, which the parked member is not.
    resume_notify: Option<Box<dyn FnOnce(ApprovalDecision) + Send>>,
}

impl std::fmt::Debug for PendingApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingApproval")
            .field("sender", &"oneshot::Sender<ApprovalDecision>")
            .field("snapshot", &self.snapshot)
            .field("resume_notify", &self.resume_notify.is_some())
            .finish()
    }
}

/// Gate for tool-approval requests that need a human or override decision.
///
/// With `auto_approve` enabled (headless mode) every request resolves to
/// [`ApprovalDecision::Approved`] immediately without registering a pending
/// entry or publishing events. Otherwise a request registers a pending
/// `oneshot` channel, publishes [`SessionEvent::ToolApprovalRequested`]
/// (when an event bus is attached), and awaits the decision posted by
/// [`ApprovalGate::resolve`].
#[derive(Debug)]
pub struct ApprovalGate {
    /// Pending approval requests, keyed by their `request_id`.
    pending: Arc<Mutex<HashMap<Uuid, PendingApproval>>>,
    /// Whether auto-approve is enabled (headless mode).
    auto_approve: bool,
    /// Optional event bus; when attached, the gate publishes
    /// `ToolApprovalRequested` / `ToolApprovalResolved` events.
    event_bus: Option<Arc<EventBus>>,
    /// Set to true by `drain()` — new requests after drain fail fast.
    closed: Arc<AtomicBool>,
    /// Ring of recently-resolved request ids (the
    /// `interaction.isRecentlyResolved` memory).
    resolved: ResolvedRing,
}

impl ApprovalGate {
    /// Create an approval gate without an event bus.
    pub fn new(auto_approve: bool) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            auto_approve,
            event_bus: None,
            closed: Arc::new(AtomicBool::new(false)),
            resolved: ResolvedRing::default(),
        }
    }

    /// Create an approval gate that publishes request/resolution events on
    /// the given bus, so external listeners can respond to requests.
    pub fn with_event_bus(auto_approve: bool, event_bus: Arc<EventBus>) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            auto_approve,
            event_bus: Some(event_bus),
            closed: Arc::new(AtomicBool::new(false)),
            resolved: ResolvedRing::default(),
        }
    }

    /// Request approval for a tool call (without agent attribution).
    ///
    /// Called by the engine when a tool needs approval. Returns a future
    /// that resolves when the decision is made (or times out on drain).
    ///
    /// In auto-approve mode this returns [`ApprovalDecision::Approved`]
    /// immediately. Otherwise the request is registered, a
    /// [`SessionEvent::ToolApprovalRequested`] event is published, and the
    /// future awaits [`ApprovalGate::resolve`].
    pub async fn request(&self, tool_name: &str, args_summary: &str) -> ApprovalDecision {
        self.request_for_agent(Uuid::nil(), tool_name, args_summary)
            .await
    }

    /// Request approval for a tool call on behalf of a specific agent.
    ///
    /// Like [`ApprovalGate::request`], but the published
    /// [`SessionEvent::ToolApprovalRequested`] event carries the agent's
    /// UUID so external listeners can attribute the request. Pass
    /// `Uuid::nil()` when the agent is unknown.
    pub async fn request_for_agent(
        &self,
        agent_id: Uuid,
        tool_name: &str,
        args_summary: &str,
    ) -> ApprovalDecision {
        if self.auto_approve {
            tracing::debug!(tool_name, "tool call auto-approved (headless mode)");
            return ApprovalDecision::Approved;
        }

        // Fail fast if the gate has been drained (session closed).
        if self.closed.load(Ordering::Relaxed) {
            return ApprovalDecision::Timeout;
        }

        let snapshot = ApprovalSnapshot {
            tool_name: tool_name.to_string(),
            action: format!("execute {tool_name}"),
            display: json!({}),
            agent_id,
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
            approval_rule: None,
            grant_dir: None,
        };
        let (request_id, receiver) = self.register(snapshot);

        tracing::debug!(
            request_id = %request_id,
            tool_name,
            args_summary,
            "tool approval requested"
        );

        match receiver.await {
            Ok(decision) => decision,
            Err(_recv_error) => {
                tracing::warn!(
                    request_id = %request_id,
                    tool_name,
                    "approval requester gave up before a decision arrived"
                );
                ApprovalDecision::Timeout
            }
        }
    }

    /// Register a pending approval request and return its minted id plus
    /// the receiver the requester awaits — the seam the reverse-RPC ask
    /// flow uses so the gate is the pending source of truth (B4-a): the
    /// ask payload carries the minted `request_id`, and both response
    /// channels (the reverse-RPC reply and the `approval.decide` RPC)
    /// converge on the returned oneshot, first one wins.
    ///
    /// A [`SessionEvent::ToolApprovalRequested`] event is published when an
    /// event bus is attached, so event-stream listeners learn the id.
    /// Unlike [`ApprovalGate::request_for_agent`] this never consults
    /// `auto_approve` — callers only reach it when a decision is genuinely
    /// needed.
    pub fn register(
        &self,
        snapshot: ApprovalSnapshot,
    ) -> (Uuid, oneshot::Receiver<ApprovalDecision>) {
        self.register_with_resume_notify(snapshot, None)
    }

    /// [`register`], with a decision-side wake callback: `resolve` invokes
    /// it (after the decision is delivered) so listeners that are not the
    /// asker's own flow — the swarm scheduler's parked member — observe the
    /// resolution without polling the asker.
    pub fn register_with_resume_notify(
        &self,
        snapshot: ApprovalSnapshot,
        resume_notify: Option<Box<dyn FnOnce(ApprovalDecision) + Send>>,
    ) -> (Uuid, oneshot::Receiver<ApprovalDecision>) {
        let request_id = Uuid::new_v4();
        let (sender, receiver) = oneshot::channel();
        let (tool_name, agent_id) = (snapshot.tool_name.clone(), snapshot.agent_id);
        {
            let mut pending = self.pending.lock().expect("approval gate mutex poisoned");
            pending.insert(
                request_id,
                PendingApproval {
                    sender,
                    snapshot,
                    resume_notify,
                },
            );
        }

        if let Some(bus) = &self.event_bus {
            bus.publish(SessionEvent::ToolApprovalRequested {
                agent_id,
                tool_name: tool_name.clone(),
                request_id,
            });
        }

        tracing::debug!(request_id = %request_id, tool_name, "tool approval requested");

        (request_id, receiver)
    }

    /// Resolve a pending approval request, waking the awaiting requester.
    ///
    /// Called by an external responder (TUI, API handler). Returns `true`
    /// when a pending request with the given `request_id` existed and was
    /// resolved; `false` for unknown or already-resolved ids.
    pub fn resolve(&self, request_id: Uuid, decision: ApprovalDecision) -> bool {
        let entry = {
            let mut pending = self.pending.lock().expect("approval gate mutex poisoned");
            pending.remove(&request_id)
        };

        match entry {
            None => false,
            Some(PendingApproval {
                sender,
                resume_notify,
                ..
            }) => {
                self.resolved.record(request_id);
                let approved = matches!(
                    decision,
                    ApprovalDecision::Approved | ApprovalDecision::ApprovedForSession
                );
                if sender.send(decision.clone()).is_ok() {
                    if let Some(bus) = &self.event_bus {
                        bus.publish(SessionEvent::ToolApprovalResolved {
                            request_id,
                            approved,
                        });
                    }
                } else {
                    tracing::warn!(
                        request_id = %request_id,
                        "approval resolved after the requester already gave up"
                    );
                }
                // Decision-side wake (ADR-011, swarm T4): the parked
                // member's scheduler learns of the resolution here — this
                // runs in the decide RPC handler, never blocked on the
                // asker's (possibly unpolled) turn.
                if let Some(resume_notify) = resume_notify {
                    resume_notify(decision);
                }
                true
            }
        }
    }

    /// Withdraw a pending approval request without resolving it — the
    /// requester's cancellation path. The awaiting receiver resolves to
    /// [`ApprovalDecision::Timeout`] semantics (the oneshot sender is
    /// dropped); no resolution event is published and the id is not
    /// recorded as resolved. Returns `true` when a pending entry was
    /// removed.
    pub fn withdraw(&self, request_id: Uuid) -> bool {
        let entry = {
            let mut pending = self.pending.lock().expect("approval gate mutex poisoned");
            pending.remove(&request_id)
        };
        entry.is_some()
    }

    /// Whether a request with the given id is currently pending.
    pub fn is_pending(&self, request_id: Uuid) -> bool {
        self.pending
            .lock()
            .expect("approval gate mutex poisoned")
            .contains_key(&request_id)
    }

    /// Every pending approval request, as `(id, snapshot)` pairs.
    pub fn list_pending(&self) -> Vec<(Uuid, ApprovalSnapshot)> {
        self.pending
            .lock()
            .expect("approval gate mutex poisoned")
            .iter()
            .map(|(id, entry)| (*id, entry.snapshot.clone()))
            .collect()
    }

    /// The payload snapshot of one pending request, when it is still
    /// pending — the decide/respond surfaces read the snapshot (its
    /// `grant_dir`) BEFORE resolving, so the grant interpretation sees the
    /// ask's payload. `None` for an unknown or already-resolved id.
    pub fn snapshot(&self, request_id: Uuid) -> Option<ApprovalSnapshot> {
        self.pending
            .lock()
            .expect("approval gate mutex poisoned")
            .get(&request_id)
            .map(|entry| entry.snapshot.clone())
    }

    /// Whether the given request id was resolved recently (still remembered
    /// by the ring).
    pub fn is_recently_resolved(&self, request_id: Uuid) -> bool {
        self.resolved.contains(request_id)
    }

    /// Cancel all pending requests (session close).
    ///
    /// Every awaiting requester resolves to [`ApprovalDecision::Timeout`]
    /// and a `ToolApprovalResolved { approved: false }` event is published
    /// for each cancelled request. The pending map is emptied. The gate is
    /// marked as closed — subsequent `request()` calls return `Timeout`
    /// until [`ApprovalGate::reopen`] is called.
    pub async fn drain(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let pending = {
            let mut guard = self.pending.lock().expect("approval gate mutex poisoned");
            guard.drain().collect::<Vec<(Uuid, PendingApproval)>>()
        };
        let count = pending.len();
        for (request_id, entry) in pending {
            let _ = entry.sender.send(ApprovalDecision::Timeout);
            if let Some(bus) = &self.event_bus {
                bus.publish(SessionEvent::ToolApprovalResolved {
                    request_id,
                    approved: false,
                });
            }
        }
        if count > 0 {
            tracing::debug!(count, "approval gate drained");
        }
    }

    /// Reopen the gate after a drain, allowing new requests to register.
    ///
    /// Gates are per-session artifacts, so draining and reopening affects
    /// only the session that owns the gate. `SessionManager` installs fresh
    /// gates on create/resume and drops them on close; `reopen` exists for
    /// consumers that reuse a gate set after draining it.
    pub fn reopen(&self) {
        self.closed.store(false, Ordering::Relaxed);
    }

    /// Number of approval requests currently awaiting a decision.
    pub fn pending_count(&self) -> usize {
        self.pending
            .lock()
            .expect("approval gate mutex poisoned")
            .len()
    }
}

/// The payload snapshot of one pending question — the structured question
/// items plus attribution, everything `question.listPending` (and the
/// interaction kernel's question rows) needs.
#[derive(Debug, Clone)]
pub struct QuestionSnapshot {
    /// The structured question items (the klient `QuestionItem[]` shape;
    /// synthesized from the plain question string by the legacy
    /// [`QuestionGate::ask`] path).
    pub questions: Value,
    /// The requesting agent, or `Uuid::nil()` when unknown.
    pub agent_id: Uuid,
    /// The engine turn that asked, when known.
    pub turn_id: Option<u64>,
    /// The tool call that asked, when known (the `AskUserQuestion` tool).
    pub tool_call_id: Option<String>,
    /// Milliseconds since the Unix epoch when the question registered.
    pub created_at: u64,
}

/// One pending question entry: the awaiting asker's oneshot sender plus
/// the question's payload snapshot.
#[derive(Debug)]
struct PendingQuestion {
    sender: oneshot::Sender<String>,
    snapshot: QuestionSnapshot,
}

/// Gate for user questions that need an interactive answer.
///
/// Works like [`ApprovalGate`]: [`QuestionGate::ask`] registers a pending
/// `oneshot` channel and awaits the answer posted by
/// [`QuestionGate::answer`] (answering publishes
/// [`SessionEvent::QuestionAsked`] / [`SessionEvent::QuestionAnswered`]
/// when an event bus is attached). Draining cancels every pending question:
/// the awaiting `ask` futures return `None`.
#[derive(Debug)]
pub struct QuestionGate {
    /// Pending questions, keyed by their `request_id`.
    pending: Arc<Mutex<HashMap<Uuid, PendingQuestion>>>,
    /// Optional event bus; when attached, the gate publishes
    /// `QuestionAsked` / `QuestionAnswered` events.
    event_bus: Option<Arc<EventBus>>,
    /// Set to true by `drain()` — new questions after drain return `None`.
    closed: Arc<AtomicBool>,
    /// Ring of recently-resolved request ids (the
    /// `interaction.isRecentlyResolved` memory).
    resolved: ResolvedRing,
}

impl QuestionGate {
    /// Create a question gate without an event bus.
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            event_bus: None,
            closed: Arc::new(AtomicBool::new(false)),
            resolved: ResolvedRing::default(),
        }
    }

    /// Create a question gate that publishes ask/answer events on the given
    /// bus, so external listeners can respond to questions.
    pub fn with_event_bus(event_bus: Arc<EventBus>) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            event_bus: Some(event_bus),
            closed: Arc::new(AtomicBool::new(false)),
            resolved: ResolvedRing::default(),
        }
    }

    /// Ask a user question (without agent attribution) and await the answer.
    ///
    /// Returns `Some(answer)` when [`QuestionGate::answer`] delivers one,
    /// and `None` when the session closes and the question is cancelled via
    /// [`QuestionGate::drain`].
    pub async fn ask(&self, question: &str) -> Option<String> {
        self.ask_for_agent(Uuid::nil(), question).await
    }

    /// Ask a user question on behalf of a specific agent and await the
    /// answer.
    ///
    /// Like [`QuestionGate::ask`], but the published
    /// [`SessionEvent::QuestionAsked`] event carries the agent's UUID so
    /// external listeners can attribute the question. Pass `Uuid::nil()`
    /// when the agent is unknown.
    pub async fn ask_for_agent(&self, agent_id: Uuid, question: &str) -> Option<String> {
        // Fail fast if the gate has been drained (session closed).
        if self.closed.load(Ordering::Relaxed) {
            return None;
        }

        // Backward-compatible snapshot: the plain question string becomes
        // the single structured question item.
        let snapshot = QuestionSnapshot {
            questions: json!([{ "question": question }]),
            agent_id,
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
        };
        let (request_id, receiver) = self.register(snapshot);

        tracing::debug!(request_id = %request_id, question, "question asked");

        receiver.await.ok()
    }

    /// Register a pending structured question and return its minted id plus
    /// the receiver the asker awaits — the seam the `AskUserQuestion`
    /// reverse-RPC flow uses so the gate is the pending source of truth
    /// (B4-a): the `question.ask` payload carries the minted `request_id`,
    /// and both response channels (the reverse-RPC reply and the
    /// `question.answer` RPC) converge on the returned oneshot, first one
    /// wins.
    ///
    /// A [`SessionEvent::QuestionAsked`] event is published when an event
    /// bus is attached, carrying the snapshot's full question items (clients
    /// derive the display card from them; ids are minted at projection).
    pub fn register(&self, snapshot: QuestionSnapshot) -> (Uuid, oneshot::Receiver<String>) {
        let request_id = Uuid::new_v4();
        let (sender, receiver) = oneshot::channel();
        let questions = snapshot.questions.clone();
        let agent_id = snapshot.agent_id;
        {
            let mut pending = self.pending.lock().expect("question gate mutex poisoned");
            pending.insert(request_id, PendingQuestion { sender, snapshot });
        }

        if let Some(bus) = &self.event_bus {
            bus.publish(SessionEvent::QuestionAsked {
                agent_id,
                questions,
                request_id,
            });
        }

        (request_id, receiver)
    }

    /// Answer a pending question, waking the awaiting requester.
    ///
    /// Called by an external responder (TUI, API handler). Returns `true`
    /// when a pending question with the given `request_id` existed and was
    /// answered; `false` for unknown or already-answered ids.
    pub fn answer(&self, request_id: Uuid, answer: String) -> bool {
        let entry = {
            let mut pending = self.pending.lock().expect("question gate mutex poisoned");
            pending.remove(&request_id)
        };

        match entry {
            None => false,
            Some(PendingQuestion { sender, .. }) => {
                self.resolved.record(request_id);
                // Publish the resolution event unconditionally — the same
                // contract `dismiss` honours. The gate is the pending
                // source of truth: when the reverse-RPC reply settles an
                // ask first, the AskUserQuestion delivery loop returns
                // with the gate receiver dropped, and this answer-into-gate
                // call is the ONLY resolution record. Gating the event on
                // the oneshot send would hide the resolution from every
                // projection (the activity fold that clears the sidebar's
                // awaiting-answer badge via `session.work_changed`), so
                // a dead receiver still gets the published event.
                if let Some(bus) = &self.event_bus {
                    bus.publish(SessionEvent::QuestionAnswered {
                        request_id,
                        answer: answer.clone(),
                    });
                }
                if sender.send(answer).is_err() {
                    tracing::debug!(
                        request_id = %request_id,
                        "question answered after the asker already gave up \
                         (resolution recorded and published to the event bus)"
                    );
                }
                true
            }
        }
    }

    /// Dismiss a pending question without answering it.
    ///
    /// The awaiting `ask` future resolves to `None` (same as drain, but for
    /// a single question). Returns `true` if the question was pending and
    /// has been dismissed, `false` for an unknown or already-answered id.
    ///
    /// Unlike `answer`, no `QuestionAnswered` event is published — a
    /// dismissed question is not an answer. If an event bus is attached,
    /// a `QuestionDismissed` event is published so clients can track it.
    ///
    /// Every dismissed question's verbatim text is recorded in the ring
    /// (ADR-007 re-ask guard): re-asking an exactly-dismissed text is
    /// rejected by `AskUserQuestion`; the snapshot is the last place the
    /// texts are available, since the pending entry is removed here.
    pub fn dismiss(&self, request_id: Uuid) -> bool {
        let entry = {
            let mut pending = self.pending.lock().expect("question gate mutex poisoned");
            pending.remove(&request_id)
        };
        if let Some(PendingQuestion { snapshot, .. }) = entry {
            self.resolved.record(request_id);
            for text in question_texts(&snapshot.questions) {
                self.resolved.record_dismissed_text(text);
            }
            // Dropping the sender cancels the awaiting `ask` future (returns None).
            if let Some(bus) = &self.event_bus {
                bus.publish(SessionEvent::QuestionDismissed { request_id });
            }
            tracing::debug!(request_id = %request_id, "question dismissed");
            true
        } else {
            false
        }
    }

    /// Withdraw a pending question without answering it — the asker's
    /// cancellation path. The awaiting receiver resolves to `None`
    /// semantics (the oneshot sender is dropped); no `QuestionDismissed`
    /// event is published and the id is not recorded as resolved.
    /// Returns `true` when a pending entry was removed.
    pub fn withdraw(&self, request_id: Uuid) -> bool {
        let entry = {
            let mut pending = self.pending.lock().expect("question gate mutex poisoned");
            pending.remove(&request_id)
        };
        entry.is_some()
    }

    /// Whether a question with the given id is currently pending.
    pub fn is_pending(&self, request_id: Uuid) -> bool {
        self.pending
            .lock()
            .expect("question gate mutex poisoned")
            .contains_key(&request_id)
    }

    /// Every pending question, as `(id, snapshot)` pairs.
    pub fn list_pending(&self) -> Vec<(Uuid, QuestionSnapshot)> {
        self.pending
            .lock()
            .expect("question gate mutex poisoned")
            .iter()
            .map(|(id, entry)| (*id, entry.snapshot.clone()))
            .collect()
    }

    /// Whether the given request id was resolved recently (still remembered
    /// by the ring).
    pub fn is_recently_resolved(&self, request_id: Uuid) -> bool {
        self.resolved.contains(request_id)
    }

    /// Whether the given question text was dismissed recently — the
    /// ADR-007 re-ask guard's session-window memory. Only DISMISSED
    /// questions are remembered; answered ones never enter this ring, so
    /// re-asking an answered question stays allowed.
    pub fn was_recently_dismissed(&self, question_text: &str) -> bool {
        self.resolved.contains_dismissed_text(question_text)
    }

    /// Cancel all pending questions (session close).
    ///
    /// Every awaiting `ask` future returns `None`. No `QuestionAnswered`
    /// event is published — a cancelled question is not an answer. The gate
    /// is marked as closed — subsequent `ask()` calls return `None` until
    /// [`QuestionGate::reopen`] is called.
    pub async fn drain(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let pending = {
            let mut guard = self.pending.lock().expect("question gate mutex poisoned");
            guard.drain().collect::<Vec<(Uuid, PendingQuestion)>>()
        };
        let count = pending.len();
        // Dropping the senders cancels the awaiting `ask` futures, which
        // then resolve to `None`.
        drop(pending);
        if count > 0 {
            tracing::debug!(count, "question gate drained");
        }
    }

    /// Reopen the gate after a drain, allowing new questions to register.
    ///
    /// Gates are per-session artifacts, so draining and reopening affects
    /// only the session that owns the gate. `SessionManager` installs fresh
    /// gates on create/resume and drops them on close; `reopen` exists for
    /// consumers that reuse a gate set after draining it.
    pub fn reopen(&self) {
        self.closed.store(false, Ordering::Relaxed);
    }

    /// Number of questions currently awaiting an answer.
    pub fn pending_count(&self) -> usize {
        self.pending
            .lock()
            .expect("question gate mutex poisoned")
            .len()
    }
}

impl Default for QuestionGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Every question item's `question` text, in order — the verbatim texts the
/// re-ask guard remembers on dismissal (every item in a dismissed batch was
/// dismissed: the whole panel closed). Public so the `AskUserQuestion`
/// tool's preflight guard extracts call texts through the same single
/// source of truth.
pub fn question_texts(questions: &Value) -> Vec<String> {
    questions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("question").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_dismiss_pending_question_when_dismissing_known_request_id() {
        // Given a gate with one pending question and a subscribed bus
        let bus = Arc::new(EventBus::new(8));
        let gate = Arc::new(QuestionGate::with_event_bus(Arc::clone(&bus)));
        let mut rx = bus.subscribe();
        let asker = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.ask("continue?").await })
        };
        let request_id = match rx.recv().await.expect("question asked event") {
            SessionEvent::QuestionAsked { request_id, .. } => request_id,
            other => panic!("expected QuestionAsked, got {other:?}"),
        };

        // When dismissing the pending question
        let dismissed = gate.dismiss(request_id);

        // Then the dismiss succeeds, the awaiting ask resolves to None, and a
        // QuestionDismissed event is published (dismissal is not an answer)
        assert!(dismissed, "known request id must be dismissed");
        assert_eq!(asker.await.expect("ask task joins"), None);
        assert!(matches!(
            rx.recv().await.expect("question dismissed event"),
            SessionEvent::QuestionDismissed { request_id: published } if published == request_id
        ));
    }

    #[tokio::test]
    async fn should_report_unknown_id_when_dismissing_unregistered_question() {
        // Given a gate without the addressed question
        let gate = QuestionGate::new();

        // When dismissing an unregistered request id
        let dismissed = gate.dismiss(Uuid::new_v4());

        // Then it is reported as not dismissed and no event is published
        assert!(!dismissed);
    }

    /// Given a pending question whose asker already dropped its receiver
    /// (the reverse-RPC reply won the race — the AskUserQuestion delivery
    /// loop returns with the gate receiver abandoned), when the answer is
    /// recorded into the gate, then a `QuestionAnswered` event is STILL
    /// published: the gate is the pending source of truth, and its
    /// projections (the session activity fold that clears the sidebar's
    /// awaiting-answer badge via `session.work_changed`) must observe every
    /// resolution regardless of whether the original asker is still
    /// listening. Mirrors `dismiss`, which publishes unconditionally.
    #[tokio::test]
    async fn should_publish_question_answered_when_asker_receiver_already_dropped() {
        // Given a gate with one pending question and a subscribed bus
        let bus = Arc::new(EventBus::new(8));
        let gate = Arc::new(QuestionGate::with_event_bus(Arc::clone(&bus)));
        let mut rx = bus.subscribe();
        let (request_id, receiver) = gate.register(QuestionSnapshot {
            questions: json!([{ "question": "Which?" }]),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
        });
        // Consume the registration event so the assertion below reads the
        // answer event, not the ask event.
        assert!(matches!(
            rx.try_recv(),
            Ok(SessionEvent::QuestionAsked { .. })
        ));
        // The asker gave up (the reverse-RPC reply settled the ask first) —
        // the receiver is dropped before the answer is recorded.
        drop(receiver);

        // When the answer is recorded into the gate
        let answered = gate.answer(request_id, "A".to_string());

        // Then the answer is recorded AND the QuestionAnswered event is
        // published anyway — the pending state and its projections clear.
        assert!(answered, "recording the answer must succeed");
        assert!(matches!(
            rx.try_recv(),
            Ok(SessionEvent::QuestionAnswered { request_id: published, .. }) if published == request_id
        ));
        assert!(
            !gate.is_pending(request_id),
            "the pending entry must be gone"
        );
    }

    /// Given a structured question registered through the B4-a seam, when
    /// list_pending reads the gate, then the snapshot carries the question
    /// items and a real createdAt timestamp.
    #[test]
    fn should_list_pending_snapshots_when_structured_question_registered() {
        // Given a gate with one structured pending question
        let gate = QuestionGate::new();
        let before = now_ms();
        let (request_id, _receiver) = gate.register(QuestionSnapshot {
            questions: json!([{ "question": "Which?" }]),
            agent_id: Uuid::nil(),
            turn_id: Some(7),
            tool_call_id: Some("call_q".to_string()),
            created_at: now_ms(),
        });

        // When the pending list is read
        let pending = gate.list_pending();

        // Then the one entry carries the id, the structured items, the
        // attribution, and a real timestamp
        assert_eq!(pending.len(), 1);
        let (id, snapshot) = &pending[0];
        assert_eq!(*id, request_id);
        assert_eq!(snapshot.questions, json!([{ "question": "Which?" }]));
        assert_eq!(snapshot.turn_id, Some(7));
        assert_eq!(snapshot.tool_call_id.as_deref(), Some("call_q"));
        assert!(snapshot.created_at >= before);
    }

    /// Given a resolved approval, when the resolved ring is queried, then
    /// the id is remembered as recently resolved — and an unknown id is not.
    #[test]
    fn should_record_resolved_ids_in_the_ring_when_approval_resolved() {
        // Given an approval gate with one pending and one resolved request
        let gate = ApprovalGate::new(false);
        let (pending_id, _rx) = gate.register(ApprovalSnapshot {
            tool_name: "Bash".to_string(),
            action: "execute Bash".to_string(),
            display: json!({}),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: Some("c1".to_string()),
            created_at: now_ms(),
            approval_rule: None,
            grant_dir: None,
        });
        let (resolved_id, _rx) = gate.register(ApprovalSnapshot {
            tool_name: "Bash".to_string(),
            action: "execute Bash".to_string(),
            display: json!({}),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: Some("c2".to_string()),
            created_at: now_ms(),
            approval_rule: None,
            grant_dir: None,
        });
        assert!(gate.resolve(resolved_id, ApprovalDecision::Approved));

        // When the ring is queried
        // Then the resolved id is recently resolved, the pending one and an
        // unknown one are not
        assert!(gate.is_recently_resolved(resolved_id));
        assert!(!gate.is_recently_resolved(pending_id));
        assert!(!gate.is_recently_resolved(Uuid::new_v4()));
    }

    /// Given a withdrawn (cancelled) registration, when resolve arrives
    /// late, then it reports not-found — the first resolution channel won.
    #[test]
    fn should_answer_false_when_resolving_a_withdrawn_request() {
        // Given a registered approval that the asker cancelled
        let gate = ApprovalGate::new(false);
        let (request_id, _rx) = gate.register(ApprovalSnapshot {
            tool_name: "Bash".to_string(),
            action: "execute Bash".to_string(),
            display: json!({}),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
            approval_rule: None,
            grant_dir: None,
        });
        assert!(gate.withdraw(request_id));

        // When the decision arrives late through the other channel
        let resolved = gate.resolve(request_id, ApprovalDecision::Approved);

        // Then the gate reports the request as already gone
        assert!(!resolved);
    }

    /// Given a dismissed structured question, when the dismissed-text ring
    /// is queried, then the verbatim text is remembered as recently
    /// dismissed — and a text that was never asked is not (ADR-007 re-ask
    /// guard: only actually-dismissed questions reject a re-ask).
    #[test]
    fn should_remember_dismissed_question_text_when_question_dismissed() {
        // Given a gate with one pending question that the user dismisses
        let gate = QuestionGate::new();
        let (request_id, _receiver) = gate.register(QuestionSnapshot {
            questions: json!([{ "question": "Which approach?" }]),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
        });
        assert!(gate.dismiss(request_id));

        // When the dismissed-text ring is queried
        // Then the verbatim text is recently dismissed, never-asked ones are not
        assert!(gate.was_recently_dismissed("Which approach?"));
        assert!(!gate.was_recently_dismissed("What approach?"));
        assert!(!gate.was_recently_dismissed("Timeline?"));
    }

    /// Given an answered question, when the dismissed-text ring is queried,
    /// then the text is NOT remembered as dismissed — answering never feeds
    /// the re-ask guard, so re-asking an answered question stays allowed
    /// (SDR-005 (d): the guard fires only for dismissed questions).
    #[test]
    fn should_not_remember_answered_question_text_when_question_answered() {
        // Given a gate with one pending question that the user answers
        let gate = QuestionGate::new();
        let (request_id, _receiver) = gate.register(QuestionSnapshot {
            questions: json!([{ "question": "Which approach?" }]),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
        });
        assert!(gate.answer(request_id, "A".to_string()));

        // When the dismissed-text ring is queried
        // Then the answered text is not recently dismissed
        assert!(!gate.was_recently_dismissed("Which approach?"));
    }

    /// Given a multi-question batch dismissed together, when the
    /// dismissed-text ring is queried, then every item's text is remembered
    /// — the whole panel was closed, so every question in it counts as
    /// dismissed for the re-ask guard.
    #[test]
    fn should_remember_every_question_text_when_multi_question_batch_dismissed() {
        // Given a gate with a two-question batch that the user dismisses
        let gate = QuestionGate::new();
        let (request_id, _receiver) = gate.register(QuestionSnapshot {
            questions: json!([
                { "question": "Which approach?" },
                { "question": "Timeline?" },
            ]),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
        });
        assert!(gate.dismiss(request_id));

        // When the dismissed-text ring is queried per item
        // Then every dismissed item's verbatim text is remembered
        assert!(gate.was_recently_dismissed("Which approach?"));
        assert!(gate.was_recently_dismissed("Timeline?"));
    }

    /// Given more dismissed question texts than the ring's capacity, when
    /// the ring is queried, then the OLDEST texts are forgotten (evicted)
    /// while the NEWEST are still remembered — the session-window "recently"
    /// memory stays bounded (mirrors the resolved-id ring's eviction).
    #[test]
    fn should_evict_oldest_texts_when_dismissed_ring_exceeds_capacity() {
        // Given a gate whose dismissed-text ring is filled past capacity
        let gate = QuestionGate::new();
        let capacity = ResolvedRing::CAPACITY;
        for i in 0..capacity {
            let (request_id, _receiver) = gate.register(QuestionSnapshot {
                questions: json!([{ "question": format!("question {i}") }]),
                agent_id: Uuid::nil(),
                turn_id: None,
                tool_call_id: None,
                created_at: now_ms(),
            });
            assert!(gate.dismiss(request_id));
        }

        // When one more text is dismissed (pushing past the capacity)
        let (request_id, _receiver) = gate.register(QuestionSnapshot {
            questions: json!([{ "question": "question overflow" }]),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: None,
            created_at: now_ms(),
        });
        assert!(gate.dismiss(request_id));

        // Then the oldest text is forgotten and the newest is remembered
        assert!(
            !gate.was_recently_dismissed("question 0"),
            "the oldest dismissed text must be evicted past the capacity"
        );
        assert!(
            gate.was_recently_dismissed("question overflow"),
            "the newest dismissed text must stay remembered"
        );
    }

    /// Given a registered approval, when it is resolved with a session-scoped
    /// approval, then the awaiting receiver gets `ApprovedForSession` (not plain
    /// `Approved`), and the gate reports the id as resolved.
    #[tokio::test]
    async fn should_deliver_session_scoped_variant_when_resolved_with_approved_for_session() {
        // Given an approval gate with one pending request
        let gate = ApprovalGate::new(false);
        let (request_id, receiver) = gate.register(ApprovalSnapshot {
            tool_name: "Bash".to_string(),
            action: "execute Bash".to_string(),
            display: json!({}),
            agent_id: Uuid::nil(),
            turn_id: None,
            tool_call_id: Some("c1".to_string()),
            created_at: now_ms(),
            approval_rule: Some("Bash".to_string()),
            grant_dir: None,
        });

        // When the decision arrives with session scope
        assert!(gate.resolve(request_id, ApprovalDecision::ApprovedForSession));

        // Then the awaiting side receives the session-scoped variant
        let decision = receiver.await.expect("the receiver resolves");
        assert!(matches!(decision, ApprovalDecision::ApprovedForSession));
        assert!(gate.is_recently_resolved(request_id));
    }
}
