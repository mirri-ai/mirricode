//! Session and agent lifecycle hooks with priority-ordered execution.
//!
//! This module provides [`OrderedHookSlot`], a chain-of-responsibility container
//! that executes registered callbacks in priority order (ascending: lower
//! numbers run first). [`SessionLifecycleHooks`] uses two slots to fire
//! callbacks when sessions are created and closed.
//!
//! # Priority convention
//!
//! | Priority range | Meaning          |
//! |----------------|------------------|
//! | negative       | "before" hooks   |
//! | 0 (default)    | normal hooks     |
//! | positive       | "after" hooks    |

use std::sync::Arc;

use tokio::sync::Mutex;

/// Type alias for a boxed callback that receives a session/agent ID as `&str`.
pub type SessionHookCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Type alias for a slot of session hooks.
pub type SessionHookSlot = OrderedHookSlot<SessionHookCallback>;

/// A single registered hook in an [`OrderedHookSlot`].
struct OrderedHook<F> {
    id: String,
    priority: i32,
    callback: F,
}

/// An ordered, priority-sorted slot for hooks of the same category.
///
/// Hooks are sorted by `priority` (ascending) at registration time, so
/// [`OrderedHookSlot::execute`] iterates them in the correct order without
/// re-sorting on every call.
///
/// The callback type `F` should be `Fn(&str) + Send + Sync` (the `&str`
/// argument is the session or agent identifier). For trait-object callbacks,
/// use `Box<dyn Fn(&str) + Send + Sync>`.
pub struct OrderedHookSlot<F> {
    hooks: Vec<OrderedHook<F>>,
}

impl<F> OrderedHookSlot<F> {
    /// Create an empty slot.
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Register a hook with the given `id` and `priority`.
    ///
    /// If a hook with the same `id` already exists, it is replaced.
    /// The slot is re-sorted by `priority` (ascending) after insertion.
    pub fn register(&mut self, id: String, priority: i32, callback: F) {
        // Remove any existing hook with the same id (replace semantics).
        self.hooks.retain(|h| h.id != id);
        self.hooks.push(OrderedHook {
            id,
            priority,
            callback,
        });
        // Sort ascending: negative (before) → 0 (default) → positive (after).
        self.hooks.sort_by_key(|h| h.priority);
    }

    /// Remove the hook with the given `id`. No-op if the id is not found.
    pub fn unregister(&mut self, id: &str) {
        self.hooks.retain(|h| h.id != id);
    }

    /// Return the number of registered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Return `true` if no hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

impl<F> Default for OrderedHookSlot<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Fn(&str) + Send + Sync> OrderedHookSlot<F> {
    /// Execute all hooks in priority order, passing `arg` to each callback.
    pub fn execute(&self, arg: &str) {
        for hook in &self.hooks {
            (hook.callback)(arg);
        }
    }
}

/// Session lifecycle hooks — fire callbacks when sessions are created or
/// closed.
///
/// Each field is an [`OrderedHookSlot`] so multiple callbacks can be
/// registered with different priorities. The slots use `Box<dyn Fn(&str)>`
/// callbacks where the `&str` argument is the session ID.
///
/// # Interior mutability
///
/// `register` and `unregister` take `&self` (via interior mutability with
/// [`Mutex`]) so hooks can be registered from async contexts without needing
/// `&mut` access to the `SessionLifecycleHooks` struct.
pub struct SessionLifecycleHooks {
    pub on_did_create_session: Arc<Mutex<SessionHookSlot>>,
    pub on_will_close_session: Arc<Mutex<SessionHookSlot>>,
}

impl SessionLifecycleHooks {
    /// Create a new `SessionLifecycleHooks` with empty slots.
    pub fn new() -> Self {
        Self {
            on_did_create_session: Arc::new(Mutex::new(SessionHookSlot::new())),
            on_will_close_session: Arc::new(Mutex::new(SessionHookSlot::new())),
        }
    }

    /// Register a callback for session creation.
    ///
    /// The callback receives the session ID as `&str`.
    /// Hooks with the same `id` are replaced. Hooks are executed in ascending
    /// priority order.
    pub async fn on_session_created(
        &self,
        id: String,
        priority: i32,
        callback: SessionHookCallback,
    ) {
        let mut slot = self.on_did_create_session.lock().await;
        slot.register(id, priority, callback);
    }

    /// Register a callback for session closing.
    ///
    /// The callback receives the session ID as `&str`.
    /// Hooks with the same `id` are replaced. Hooks are executed in ascending
    /// priority order.
    pub async fn on_session_closing(
        &self,
        id: String,
        priority: i32,
        callback: SessionHookCallback,
    ) {
        let mut slot = self.on_will_close_session.lock().await;
        slot.register(id, priority, callback);
    }

    /// Fire all session-created callbacks with the given `session_id`.
    pub async fn fire_session_created(&self, session_id: &str) {
        let slot = self.on_did_create_session.lock().await;
        slot.execute(session_id);
    }

    /// Fire all session-closing callbacks with the given `session_id`.
    pub async fn fire_session_closing(&self, session_id: &str) {
        let slot = self.on_will_close_session.lock().await;
        slot.execute(session_id);
    }
}

impl Default for SessionLifecycleHooks {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn should_contain_registered_hook_when_registering_into_empty_slot() {
        let mut slot: SessionHookSlot = OrderedHookSlot::new();
        assert!(slot.is_empty());

        slot.register("h1".to_string(), 0, Box::new(|_| {}));
        assert_eq!(slot.len(), 1);
        assert!(!slot.is_empty());
    }

    #[test]
    fn should_replace_existing_hook_when_registering_duplicate_id() {
        let mut slot: SessionHookSlot = OrderedHookSlot::new();
        slot.register("h1".to_string(), 0, Box::new(|_| {}));
        slot.register("h1".to_string(), 0, Box::new(|_| {}));
        assert_eq!(slot.len(), 1, "duplicate id should replace, not add");
    }

    #[test]
    fn should_remove_hook_when_unregistering_registered_id() {
        let mut slot: SessionHookSlot = OrderedHookSlot::new();
        slot.register("h1".to_string(), 0, Box::new(|_| {}));
        slot.register("h2".to_string(), 0, Box::new(|_| {}));
        assert_eq!(slot.len(), 2);

        slot.unregister("h1");
        assert_eq!(slot.len(), 1);
    }

    #[test]
    fn should_leave_slot_unchanged_when_unregistering_unknown_id() {
        let mut slot: SessionHookSlot = OrderedHookSlot::new();
        slot.register("h1".to_string(), 0, Box::new(|_| {}));
        slot.unregister("nonexistent");
        assert_eq!(slot.len(), 1);
    }

    #[test]
    fn should_execute_hooks_in_ascending_priority_order_when_executing() {
        let mut slot: SessionHookSlot = OrderedHookSlot::new();
        let order = Arc::new(StdMutex::new(Vec::<String>::new()));

        let order_a = Arc::clone(&order);
        slot.register(
            "after".to_string(),
            10,
            Box::new(move |_| {
                order_a.lock().unwrap().push("after".to_string());
            }),
        );

        let order_b = Arc::clone(&order);
        slot.register(
            "before".to_string(),
            -10,
            Box::new(move |_| {
                order_b.lock().unwrap().push("before".to_string());
            }),
        );

        let order_c = Arc::clone(&order);
        slot.register(
            "default".to_string(),
            0,
            Box::new(move |_| {
                order_c.lock().unwrap().push("default".to_string());
            }),
        );

        slot.execute("session-1");

        let recorded = order.lock().unwrap();
        assert_eq!(
            *recorded,
            vec!["before", "default", "after"],
            "hooks should execute in ascending priority order"
        );
    }

    #[tokio::test]
    async fn should_run_callbacks_when_firing_session_created() {
        let hooks = SessionLifecycleHooks::new();
        let fired = Arc::new(StdMutex::new(Vec::<String>::new()));

        let fired_clone = Arc::clone(&fired);
        hooks
            .on_session_created(
                "log".to_string(),
                0,
                Box::new(move |sid| {
                    fired_clone.lock().unwrap().push(sid.to_string());
                }),
            )
            .await;

        hooks.fire_session_created("session-42").await;

        let recorded = fired.lock().unwrap();
        assert_eq!(*recorded, vec!["session-42".to_string()]);
    }

    #[tokio::test]
    async fn should_run_callbacks_when_firing_session_closing() {
        let hooks = SessionLifecycleHooks::new();
        let fired = Arc::new(StdMutex::new(false));

        let fired_clone = Arc::clone(&fired);
        hooks
            .on_session_closing(
                "cleanup".to_string(),
                0,
                Box::new(move |_| {
                    *fired_clone.lock().unwrap() = true;
                }),
            )
            .await;

        hooks.fire_session_closing("session-99").await;

        assert!(*fired.lock().unwrap(), "closing callback should have fired");
    }

    #[tokio::test]
    async fn should_not_fire_unregistered_callback_when_firing_session_created() {
        let hooks = SessionLifecycleHooks::new();
        let count = Arc::new(StdMutex::new(0u32));

        let count_clone = Arc::clone(&count);
        hooks
            .on_session_created(
                "counter".to_string(),
                0,
                Box::new(move |_| {
                    *count_clone.lock().unwrap() += 1;
                }),
            )
            .await;

        hooks.fire_session_created("s1").await;
        assert_eq!(*count.lock().unwrap(), 1);

        // Unregister by directly accessing the slot.
        {
            let mut slot = hooks.on_did_create_session.lock().await;
            slot.unregister("counter");
        }

        hooks.fire_session_created("s2").await;
        assert_eq!(
            *count.lock().unwrap(),
            1,
            "callback should not fire after unregister"
        );
    }

    #[tokio::test]
    async fn should_fire_hooks_in_priority_order_when_multiple_created_hooks_registered() {
        let hooks = SessionLifecycleHooks::new();
        let order = Arc::new(StdMutex::new(Vec::<String>::new()));

        let o1 = Arc::clone(&order);
        hooks
            .on_session_created(
                "after".to_string(),
                10,
                Box::new(move |_| {
                    o1.lock().unwrap().push("after".to_string());
                }),
            )
            .await;

        let o2 = Arc::clone(&order);
        hooks
            .on_session_created(
                "before".to_string(),
                -10,
                Box::new(move |_| {
                    o2.lock().unwrap().push("before".to_string());
                }),
            )
            .await;

        hooks.fire_session_created("session-x").await;

        let recorded = order.lock().unwrap();
        assert_eq!(
            *recorded,
            vec!["before", "after"],
            "session-created hooks should fire in priority order"
        );
    }
}
