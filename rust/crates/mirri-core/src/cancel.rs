use std::sync::Mutex;

use tokio_util::sync::CancellationToken;

use crate::error::{EngineError, EngineResult};

/// Why a [`CancelToken`] was cancelled.
///
/// This distinguishes user-initiated cancellation (e.g. the user pressed
/// Ctrl-C or clicked "stop") from system-initiated cancellation (e.g. a
/// scope shutdown or timeout). The reason is surfaced via
/// [`EngineError::UserCancelled`] or [`EngineError::Aborted`] when the
/// cancelled operation is awaited via [`CancelToken::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelReason {
    /// Cancelled by the system (scope shutdown, timeout, etc.).
    System,
    /// Cancelled by the user (Ctrl-C, "stop" button, etc.).
    User,
    /// Cancelled by the stall watchdog — the turn was judged silent past
    /// the inactivity window (ADR-004). Distinct from a plain system
    /// cancel so the settle path can report the `stalled` outcome.
    Stalled,
}

/// Cancellation handle, equivalent to TypeScript's AbortSignal/AbortController.
///
/// Cloning a `CancelToken` creates a new handle to the same cancellation
/// source. When the source is cancelled, all clones observe cancellation.
///
/// A `child()` token is linked to its parent: cancelling the parent cancels
/// the child, but cancelling the child does NOT cancel the parent.
#[derive(Debug)]
pub struct CancelToken {
    inner: CancellationToken,
    /// The reason this token was cancelled (`None` until cancelled).
    /// Stored in a `Mutex` so it can be set by `cancel`/`cancel_user` and
    /// read by `check` without requiring `&mut self`.
    ///
    /// ADR-004: the reason is SHARED across clones (behind an `Arc`).
    /// The previous per-clone reason meant a watchdog cancelling one
    /// clone left every other handle of the same source blind to the
    /// stall marker — the settle path (holding a different clone) then
    /// mis-reported the outcome as a user cancel. Clones are handles to
    /// one cancellation; the reason belongs to the source, not the handle.
    reason: std::sync::Arc<Mutex<Option<CancelReason>>>,
}

impl Clone for CancelToken {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            reason: std::sync::Arc::clone(&self.reason),
        }
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            inner: CancellationToken::new(),
            reason: std::sync::Arc::new(Mutex::new(None)),
        }
    }

    /// Create a child token. Cancelling the parent cancels the child,
    /// but cancelling the child does NOT cancel the parent. The child
    /// starts with NO reason of its own (a fresh source) — a reason set
    /// later on either token is not visible on the other.
    pub fn child(&self) -> Self {
        Self {
            inner: self.inner.child_token(),
            reason: std::sync::Arc::new(Mutex::new(None)),
        }
    }

    /// Cancel the token with system-cancellation semantics. All clones and
    /// children observe cancellation. When the cancelled operation is
    /// awaited via [`check`](Self::check), it returns
    /// [`EngineError::Aborted`].
    pub fn cancel(&self) {
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(CancelReason::System);
        self.inner.cancel();
    }

    /// Cancel with user-cancellation semantics. All clones and children
    /// observe cancellation. When the cancelled operation is awaited via
    /// [`check`](Self::check), it returns [`EngineError::UserCancelled`].
    pub fn cancel_user(&self) {
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(CancelReason::User);
        self.inner.cancel();
    }

    /// Cancel with stall-watchdog semantics (ADR-004): the turn went
    /// silent past the inactivity window. Observers awaiting via
    /// [`check`](Self::check) see [`EngineError::Aborted`] like a plain
    /// system cancel; the distinction is read by the settle path via
    /// [`Self::is_stalled`] to report the `stalled` wire outcome.
    pub fn cancel_stalled(&self) {
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(CancelReason::Stalled);
        self.inner.cancel();
    }

    /// Whether the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Throw if cancelled (equivalent to `signal.throwIfAborted()`).
    ///
    /// Returns [`EngineError::UserCancelled`] if the token was cancelled via
    /// [`cancel_user`](Self::cancel_user), or [`EngineError::Aborted`] if
    /// cancelled via [`cancel`](Self::cancel) or if the reason is unknown
    /// (e.g. the token was cancelled by a parent token).
    pub fn check(&self) -> EngineResult<()> {
        if self.inner.is_cancelled() {
            let reason = *self.reason.lock().unwrap_or_else(|e| e.into_inner());
            match reason {
                Some(CancelReason::User) => Err(EngineError::UserCancelled),
                _ => Err(EngineError::Aborted("operation cancelled".into())),
            }
        } else {
            Ok(())
        }
    }

    /// Whether this token was cancelled with user-cancellation semantics
    /// (i.e. via [`cancel_user`](Self::cancel_user)) rather than system
    /// semantics ([`cancel`](Self::cancel)).
    ///
    /// The reason is shared across clones of the same source (ADR-004):
    /// a clone reports the reason another clone set. Children are
    /// separate sources — a reason set on the parent is not visible on
    /// the child.
    pub fn is_user_cancelled(&self) -> bool {
        matches!(
            *self.reason.lock().unwrap_or_else(|e| e.into_inner()),
            Some(CancelReason::User)
        )
    }

    /// Whether this token was cancelled by the stall watchdog (ADR-004):
    /// the settle path maps such turns to the `stalled` wire outcome
    /// instead of `cancelled`. The reason is shared across clones of the
    /// same source; children are separate sources.
    pub fn is_stalled(&self) -> bool {
        matches!(
            *self.reason.lock().unwrap_or_else(|e| e.into_inner()),
            Some(CancelReason::Stalled)
        )
    }

    /// Wait for cancellation. Returns immediately if already cancelled.
    pub async fn cancelled(&self) {
        self.inner.cancelled().await;
    }

    /// Get the underlying `CancellationToken` for direct use in
    /// `tokio::select!` branches.
    pub fn token(&self) -> CancellationToken {
        self.inner.clone()
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}
