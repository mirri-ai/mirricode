# Todo Parity (Rust Core) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the 7 Rust-core gaps (G1–G7) so the session todo list is a wire-persisted, resumable, undo-checkpointed, reminder-guarded session state — matching the TS v2 baseline.

**Architecture:** Writes dual-commit (wire record first, then memory) via the existing `tools.update_store { key: "todo" }` op and goal-tool `with_wire_recorder` pattern. The reducer folds todo state into a new `ToolsState.store` map (authoritative); in-memory `Arc<TodoList>` is a cache re-synced at restore/undo. Reminder assembly moves to the embedded main-agent turn flow with inject-time active-tool gating.

**Tech Stack:** Rust (tokio, serde) in `rust/crates/{mirri-proto, mirri-core, mirri-embedded, mirri-server}`; one small TS consumer change in `apps/mirri-web`.

**Spec:** `docs/specs/todo-management/` (requirements / architecture / context G1–G7 / ROADMAP F-1…F-9).

## Global Constraints

- All engine changes land in `rust/crates/` only — TS packages are maintenance-frozen (repo AGENTS.md). The single exception is the web consumer's snapshot-preference switch (F-9).
- Wire record shape: `tools.update_store` with `key: "todo"`, `value` = full `[{title, status}]` array (ADR-1). No new wire op enum variant.
- Tool contract texts (description, output strings, write-reminder) must NOT change — they are tool-calling domain contract (builtin-tools §4.4).
- Rust test convention: Given-When-Then test names (`should_..._when_...`).
- Verify with `cargo test --manifest-path rust/Cargo.toml -p <crate> <filter>` for targeted runs; full `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` only at the end (20+ min background).
- Existing tests that construct `TodoListTool::new(list)` keep compiling — the recorder is added via builder, defaulting to the empty (no-op) recorder, exactly like the goal tools.

---

### Task 1: Wire record on write (G1, F-1)

**Files:**
- Modify: `rust/crates/mirri-core/src/tools/todo_list.rs`
- Modify: `rust/crates/mirri-core/src/tools/mod.rs:328` (registration)
- Test: same file's `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `SessionWireLog::record_op(WireOp::ToolsUpdateStore { key, value })` (async, existing op in `mirri-proto/src/wire.rs:442`); `BuiltinToolServices.wire_recorder` (existing field, `tools/mod.rs:227`).
- Produces: `TodoListTool::new(list).with_wire_recorder(recorder)` builder (same shape as `UpdateGoalTool`, `tools/update_goal.rs:51`). Every confirmed write appends one wire op before mutating memory.

- [ ] **Step 1: Write the failing tests**

Add to `tools/todo_list.rs` tests module (after the existing `should_update_and_return_list_with_reminder_when_todos_is_provided`):

```rust
    #[tokio::test]
    async fn should_record_a_wire_op_when_writing_todos() {
        let list = Arc::new(TodoList::new());
        let recorder = crate::scope::SessionWireLog::default();
        let tool = TodoListTool::new(Arc::clone(&list)).with_wire_recorder(recorder.clone());
        let input = serde_json::json!({
            "todos": [{"title": "Task A", "status": "pending"}]
        });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error);
        // The deferred slot buffers the op — read it back.
        let ops = recorder.take_deferred_ops();
        assert_eq!(ops.len(), 1, "exactly one wire op per write");
    }

    #[tokio::test]
    async fn should_not_record_a_wire_op_when_querying_todos() {
        let list = Arc::new(TodoList::new());
        let recorder = crate::scope::SessionWireLog::default();
        let tool = TodoListTool::new(Arc::clone(&list)).with_wire_recorder(recorder.clone());
        let result = execute_tool(&tool, serde_json::json!({})).await;
        assert!(!result.is_error);
        assert!(recorder.take_deferred_ops().is_empty(), "query records nothing");
    }
```

Note: if `take_deferred_ops` does not exist on `SessionWireLog`, add a `#[cfg(test)]`-gated accessor in `rust/crates/mirri-core/src/scope/session_scope.rs` that drains the `WireLogSlot::Deferred` buffer under the writer lock (the slot enum is private; add the method next to `start_deferred`, ~line 143). This is test plumbing only.

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core tools::todo_list -- --nocapture 2>&1 | tail -5
```

Expected: compile error `no method named with_wire_recorder found for TodoListTool` / `take_deferred_ops`.

- [ ] **Step 3: Implement**

In `tools/todo_list.rs`:

```rust
pub struct TodoListTool {
    todo_list: Arc<TodoList>,
    wire_recorder: Option<crate::scope::SessionWireLog>,
}

impl TodoListTool {
    pub fn new(todo_list: Arc<TodoList>) -> Self {
        Self { todo_list, wire_recorder: None }
    }

    /// Attach the session's wire-log recorder so writes persist as
    /// `tools.update_store` (key "todo") records.
    pub fn with_wire_recorder(mut self, recorder: crate::scope::SessionWireLog) -> Self {
        self.wire_recorder = Some(recorder);
        self
    }
}
```

In the write-mode `execute` closure (before `todo_list.set(items)`), serialize and record:

```rust
let wire_value: Vec<serde_json::Value> = items
    .iter()
    .map(|t| serde_json::json!({ "title": t.title, "status": t.status.as_str() }))
    .collect();
if let Some(recorder) = wire_recorder.as_ref() {
    recorder
        .record_op(WireOp::ToolsUpdateStore {
            key: "todo".to_string(),
            value: serde_json::Value::Array(wire_value),
        })
        .await;
}
```

(Move `items` into the closure as today; compute `wire_value` inside the closure before `set`.) Import `WireOp` from `mirri_proto` the same way `update_goal.rs` does.

In `tools/mod.rs:328` change registration to:

```rust
tool_manager.register_builtin(Arc::new(
    TodoListTool::new(Arc::clone(&services.todo_list))
        .with_wire_recorder(services.wire_recorder.clone()),
));
```

- [ ] **Step 4: Run tests — pass**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core tools::todo_list
```

Expected: all green, including the 5 pre-existing tests (empty recorder default keeps them compiling).

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/tools/todo_list.rs rust/crates/mirri-core/src/tools/mod.rs
git commit -m "feat(todo): record tools.update_store wire op on TodoList write"
```

---

### Task 2: Reducer fold + state field (G2 core, F-2 part 1)

**Files:**
- Modify: `rust/crates/mirri-proto/src/state.rs:333-361` (`ToolsState`)
- Modify: `rust/crates/mirri-proto/src/reducer.rs` (Tools domain, after `ToolsSetUserToolEnabled` arm ~line 665)
- Test: `rust/crates/mirri-proto/src/reducer.rs` test module (follow existing reducer test placement)

**Interfaces:**
- Consumes: `WireOp::ToolsUpdateStore { key: String, value: serde_json::Value }`.
- Produces: `ToolsState { active, user_registrations, disabled_user_tools, store: HashMap<String, serde_json::Value> }` — serde `#[serde(default, skip_serializing_if = "HashMap::is_empty")]`. Later tasks read `state.tools.store.get("todo")`.
- Produces: todo item validation helper `pub fn parse_todo_items(value: &serde_json::Value) -> Option<Vec<(String, String)>>` in state.rs or reducer.rs — returns `Some(vec!)` only if `value` is an array whose every element has string `title` (non-empty) and `status` ∈ {pending, in_progress, done} (case-insensitive, matching `TodoStatus::from_str` tolerance); `None` on any malformation INCLUDING non-array. Later tasks (restore, undo resync) reuse it.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn should_store_the_todo_value_when_folding_an_update_store_op_with_todo_key() {
    let mut state = AgentState::default();
    let op = WireOp::ToolsUpdateStore {
        key: "todo".to_string(),
        value: serde_json::json!([{ "title": "a", "status": "pending" }]),
    };
    reduce_op(&mut state, &op);
    assert_eq!(
        state.tools.store.get("todo"),
        Some(&serde_json::json!([{ "title": "a", "status": "pending" }]))
    );
}

#[test]
fn should_keep_the_previous_value_when_folding_a_malformed_todo_store_op() {
    let mut state = AgentState::default();
    reduce_op(&mut state, &WireOp::ToolsUpdateStore {
        key: "todo".to_string(),
        value: serde_json::json!([{ "title": "a", "status": "pending" }]),
    });
    // value is not an array -> skip, keep old
    reduce_op(&mut state, &WireOp::ToolsUpdateStore {
        key: "todo".to_string(),
        value: serde_json::json!("garbage"),
    });
    assert!(state.tools.store.get("todo").unwrap().is_array());
    // bad item shape -> skip, keep old
    reduce_op(&mut state, &WireOp::ToolsUpdateStore {
        key: "todo".to_string(),
        value: serde_json::json!([{ "title": 42 }]),
    });
    assert!(state.tools.store.get("todo").unwrap().is_array());
}

#[test]
fn should_store_unrelated_keys_untouched_when_folding_non_todo_store_ops() {
    let mut state = AgentState::default();
    reduce_op(&mut state, &WireOp::ToolsUpdateStore {
        key: "other".to_string(),
        value: serde_json::json!({"x": 1}),
    });
    assert!(!state.tools.store.contains_key("todo"));
    assert!(state.tools.store.contains_key("other"));
}
```

(Adapt helper/reduce-entry names to the file's existing test conventions — the entry point is the same function the other Tools-domain tests use; check neighboring tests for the exact call form, some use `apply` or a wrapper.)

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-proto update_store
```

Expected: compile error `no field store on ToolsState`.

- [ ] **Step 3: Implement**

In `state.rs` add the field + update `is_default`:

```rust
    /// Generic tool-store entries (`tools.update_store`), keyed by store
    /// key. Currently carries the session todo list (`"todo"`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub store: std::collections::HashMap<String, serde_json::Value>,
```

and `is_default` gains `&& self.store.is_empty()`.

In `reducer.rs` Tools domain:

```rust
        WireOp::ToolsUpdateStore { key, value } => {
            // TS parity (todoOps.ts): the `"todo"` key validates strictly —
            // a malformed value (non-array, bad item shape) is skipped so
            // one bad record never takes the fold down; other keys are
            // stored verbatim.
            if key == "todo" {
                if parse_todo_items(value).is_some() {
                    state.tools.store.insert(key.clone(), value.clone());
                }
            } else {
                state.tools.store.insert(key.clone(), value.clone());
            }
        }
```

And the helper (in reducer.rs near the Tools domain, exported for reuse):

```rust
/// Validate a `"todo"` store value: an array whose every item has a
/// non-empty string `title` and a `status` of pending/in_progress/done
/// (case-insensitive). Returns None on any malformation.
pub fn parse_todo_items(value: &serde_json::Value) -> Option<Vec<(String, String)>> {
    let arr = value.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let title = item.get("title")?.as_str()?;
        if title.is_empty() { return None; }
        let status = item.get("status")?.as_str()?.to_ascii_lowercase();
        if !matches!(status.as_str(), "pending" | "in_progress" | "done") {
            return None;
        }
        out.push((title.to_string(), status));
    }
    Some(out)
}
```

- [ ] **Step 4: Run tests — pass**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-proto
```

Expected: full crate green (the field is `default` + `skip_serializing_if`, so existing state round-trip tests are unaffected).

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-proto/src/state.rs rust/crates/mirri-proto/src/reducer.rs
git commit -m "feat(proto): fold tools.update_store into ToolsState.store with todo validation"
```

---

### Task 3: Restore on resume (G2 core, F-2 part 2 + F-8)

**Files:**
- Modify: `rust/crates/mirri-core/src/session/manager/restore.rs` (`restore_session_scoped_state`, after the cron restore at ~line 1260)
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs` (`resumed_session_summary`, agents map construction ~line 880)
- Test: `rust/crates/mirri-core/tests/session_resume_tests.rs` (follow file conventions)

**Interfaces:**
- Consumes: `parse_todo_items` (Task 2); `AgentState.tools.store`; `agent.todo_list()` accessor (`scope/agent_scope.rs:331`).
- Produces: after resume, `session.todo_list()` is populated and the resume response's main-agent entry carries `"toolStore": { "todo": [...] }`.

- [ ] **Step 1: Write the failing test**

In `session_resume_tests.rs` (mirror the file's existing harness — reuse its wire-log writer helper `write_wire_log` and session-creation helpers; see the `should_restore_disabled_user_tool_when_resuming_session` test in `session/manager/tests.rs:1414` for the state-restore assertion pattern):

```rust
#[tokio::test]
async fn should_restore_the_todo_list_when_resuming_a_session() {
    // Given a session whose wire log carries one tools.update_store(todo) op
    // (built with the file's existing wire-log construction helpers)
    // When the session is resumed
    // Then the resumed session's todo_list() holds the recorded items
    //   - 2 items, statuses preserved
    // And the resume response's main agent carries toolStore['todo']
}
```

Write the concrete body using the neighboring test's helper calls — the exact scaffold (temp dir, `SessionManager`, `write_wire_log` with a `ToolsUpdateStore` op line, resume, assertions on `session.todo_list().get()` and the summary's `agents["main"]["toolStore"]["todo"]`) follows `should_recreate_agents_when_closed_session_resumed` (`session_resume_tests.rs:509`) line for line, substituting the wire op.

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core --test session_resume_tests should_restore_the_todo_list
```

Expected: assertion failure — `todo_list()` empty / `toolStore` missing.

- [ ] **Step 3: Implement**

In `restore_session_scoped_state` (restore.rs, after the cron restore call at line 1260):

```rust
    // Restore the session todo list replayed from `tools.update_store`
    // (key 'todo'). The fold validates strictly; a malformed stored value
    // (should be impossible via the reducer's own gate, but a hand-edited
    // log can) is skipped with a warning rather than failing the resume.
    if let Some(value) = state.tools.store.get("todo") {
        match parse_todo_items(value) {
            Some(items) => {
                agent.todo_list().set(
                    items.into_iter()
                        .map(|(title, status)| crate::todo::TodoItem {
                            title,
                            status: status.parse().expect("validated by parse_todo_items"),
                        })
                        .collect(),
                );
            }
            None => {
                tracing::warn!("skipping malformed todo store value on resume");
            }
        }
    }
```

(`parse_todo_items` is re-exported through mirri-proto's public API — add it to `mirri-proto/src/lib.rs` exports alongside the reducer types.)

In `session_handlers.rs` `resumed_session_summary`, inside the main-agent branch (where `agent_type == "main"`), add to the agent JSON map:

```rust
            // The todo panel's hydration source (the TUI replay renderer
            // reads `toolStore['todo']`): the folded wire state's todo
            // entry — the same source restore just applied to memory.
            let tool_store = serde_json::json!({
                "todo": agent.todo_list().get().iter()
                    .map(|t| serde_json::json!({ "title": t.title, "status": t.status.as_str() }))
                    .collect::<Vec<_>>(),
            });
```

and include `"toolStore": tool_store` in the agent map entry. Also update the doc comment at `session_handlers.rs:828` — remove `toolStore` from the honest-gaps list.

- [ ] **Step 4: Run tests — pass**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core --test session_resume_tests
cargo test --manifest-path rust/Cargo.toml -p mirri-embedded
```

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/session/manager/restore.rs rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs rust/crates/mirri-core/tests/session_resume_tests.rs
git commit -m "feat(todo): restore the todo list on resume and expose toolStore in the summary"
```

---

### Task 4: Undo checkpoints (G4, F-3)

**Files:**
- Modify: `rust/crates/mirri-core/src/todo.rs` (checkpoint stack in `TodoList`)
- Modify: `rust/crates/mirri-core/src/session/manager/restore.rs` (wire the resync — see below)
- Test: `rust/crates/mirri-core/src/todo.rs` tests + a resume-fold test

**Interfaces:**
- Consumes: reducer now also maintains checkpoints (Task 2's `store` holds the current value; this task adds the checkpoint list to the FOLDED state, not the memory struct).
- Produces: `TodoList::push_checkpoint()`, `TodoList::pop_checkpoint(n) -> usize`, `TodoList::clear_checkpoints()` — memory-side mirrors of the fold.

**Design decision (from arch §2.4/ADR-2):** checkpoints live in the folded state. Since `ToolsState.store` is a plain map, add a sibling field `todo_checkpoints: Vec<serde_json::Value>` to `ToolsState` (same serde attributes). The reducer arms:

```rust
        // In WireOp::ContextAppendMessage arm — after pushing the message:
        // TS parity: a real-user-input undo anchor checkpoints the todo
        // list (conversationTime.ts isUndoAnchor).
        if is_undo_anchor_message(message) {
            if let Some(current) = state.tools.store.get("todo").cloned() {
                state.tools.todo_checkpoints.push(current);
            } else {
                state.tools.todo_checkpoints.push(serde_json::Value::Array(vec![]));
            }
        }

        // New arm content for WireOp::ContextUndo — after truncation:
        if undo_count <= state.tools.todo_checkpoints.len() {
            let idx = state.tools.todo_checkpoints.len() - undo_count;
            if let Some(restored) = state.tools.todo_checkpoints.get(idx).cloned() {
                state.tools.store.insert("todo".to_string(), restored);
            }
            state.tools.todo_checkpoints.truncate(idx);
        }

        // In WireOp::FullCompactionComplete arm:
        state.tools.todo_checkpoints.clear();
```

`is_undo_anchor_message(message: &serde_json::Value) -> bool` (private helper in reducer.rs): `message.get("role") == "user"` AND (message has no `origin` key OR `origin.kind` is absent/`"user"`). This mirrors `is_undo_anchor` (`context_memory/mod.rs:1092`): user role, origin None or `is_real_user_input()` — check what `is_real_user_input` matches for the serialized origin shape and mirror it (the wire form of origin is the serialized `PromptOrigin`; `None` origin may serialize as absent or null — handle both).

- [ ] **Step 1: Write the failing tests** — in `mirri-proto` reducer tests:

```rust
#[test]
fn should_checkpoint_the_todo_list_when_a_real_user_message_is_appended() {
    // Given state with store["todo"] = [item A]
    // When ContextAppendMessage with role=user (no origin)
    // Then todo_checkpoints == [[item A]] and store unchanged
}

#[test]
fn should_restore_the_older_todo_list_when_undoing_past_a_write() {
    // Given store["todo"] = [A], checkpoint [A] pushed, then store set to [B]
    // When ContextUndo { count: 1 }
    // Then store["todo"] == [A] and checkpoints drained
}

#[test]
fn should_keep_the_todo_list_when_undo_count_exceeds_checkpoints() {
    // Given 1 checkpoint, When ContextUndo { count: 3 }, Then store unchanged
}

#[test]
fn should_clear_todo_checkpoints_when_compaction_completes() {
    // Given checkpoints non-empty, When FullCompactionComplete, Then empty
}
```

And in `mirri-core` `todo.rs` tests — memory-side `push_checkpoint/pop_checkpoint/clear_checkpoints` round-trip (Given a set list, When push + set new + pop(1), Then the original list is back).

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-proto todo_checkpoint
cargo test --manifest-path rust/Cargo.toml -p mirri-core todo::tests
```

- [ ] **Step 3: Implement** both sides (reducer arms above; memory methods in `todo.rs` guarded by the same `Mutex`).

In `restore.rs`, after the Task 3 todo restore block, resync applies automatically — the undo path in the engine (the site that processes `ContextUndo` against live memory) must call `todo_list` resync. Find the engine's undo execution site (where `ContextUndo` is dispatched to the wire — grep `WireOp::ContextUndo` in mirri-core beyond the reducer): after the undo op records, read the folded state's `store["todo"]` and `todo_list.set(...)` it (mirror the Task 3 restore block; factor it into a small `fn restore_todo_from_state(state: &AgentState, list: &Arc<TodoList>)` reused by both sites).

- [ ] **Step 4: Run tests — pass**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-proto
cargo test --manifest-path rust/Cargo.toml -p mirri-core todo
```

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-proto/src/state.rs rust/crates/mirri-proto/src/reducer.rs rust/crates/mirri-core/src/todo.rs rust/crates/mirri-core/src/session/manager/restore.rs
git commit -m "feat(todo): undo-checkpoint the todo list in the folded state"
```

---

### Task 5: Main-agent reminder assembly (G5, F-5)

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs` (~line 308-330, the turn-flow setter sequence)
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests/` — locate the turn-flow wiring test module (grep for `set_task_manager` in the tests dir to find the file)

**Interfaces:**
- Consumes: `TurnFlow::set_todo_list(Arc<TodoList>)` (existing, `turn_flow/mod.rs:750`); `agent.todo_list()`.
- Produces: main agent turns get the `TodoListReminderInjector` (same injector the subagent host already wires at `subagent/host.rs:1080`).

- [ ] **Step 1: Write the failing test**

In the embedded tests module that covers turn-flow wiring (the file where `set_task_manager` appears in tests):

```rust
#[tokio::test]
async fn should_wire_the_todo_reminder_injector_for_the_main_agent_turn_flow() {
    // Given a live session with a todo list containing one pending item
    // When its turn flow is created via get_or_create_turn_flow
    // Then the flow's todo injector is present (not the default None)
    //   — assert via the observable effect: run a turn-flow injection pass
    //      and check the todo reminder appears when thresholds are met,
    //      OR assert the private field via a test accessor if the module
    //      exposes one (mirror how neighboring injector tests assert).
}
```

Follow the file's existing injector-wiring test pattern exactly (grep `injector` in the embedded tests for the assertion idiom).

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-embedded todo
```

- [ ] **Step 3: Implement**

In `message_handlers.rs`, in the turn-flow setup sequence (after `turn_flow.set_task_manager(task_manager);` at line 321):

```rust
        // The session-shared todo list — the stale-todo reminder injector
        // (G5): the main agent's turns must remind on a stale list, the
        // same wiring the subagent host already does (subagent/host.rs).
        turn_flow.set_todo_list(agent.todo_list());
```

`agent` is in scope at that point (the sequence already uses `agent_hook_engine` derived from it — verify the variable name in the actual function body and use the same).

- [ ] **Step 4: Run tests — pass**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-embedded
```

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs
git commit -m "fix(todo): wire the stale-todo reminder injector into the main-agent turn flow"
```

---

### Task 6: Inject-time active-tool gating (G6, F-6)

**Files:**
- Modify: `rust/crates/mirri-core/src/turn_flow/injection/todo_list.rs`
- Modify: `rust/crates/mirri-core/src/turn_flow/mod.rs` (`set_todo_list` signature extension)
- Test: `rust/crates/mirri-core/src/turn_flow/injection/tests.rs`

**Interfaces:**
- Consumes: `ToolManager` active-tool query — find the read API for "is `TodoList` in the active set" (grep `set_active_tools` / the manager's public read methods; `ToolManager` is behind `Arc<RwLock<ToolManager>>` on the agent).
- Produces: `TodoListReminderInjector::new(todo_list)` unchanged, plus `TodoListReminderInjector::with_tool_gate(gate: Arc<dyn Fn() -> bool + Send + Sync>)`; `TurnFlow::set_todo_list` gains an optional gate parameter or a sibling setter `set_todo_tool_gate`.

**Design (arch §2.5, D3 fix):** the gate is evaluated at every `inject` call, not at construction — so a mid-session disable takes effect on the next injection.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn should_not_inject_a_reminder_when_the_todo_tool_gate_is_closed() {
    // Given history that meets the 10/10 thresholds and a non-empty list
    // When the injector's tool gate returns false
    // Then inject() returns None
}

#[test]
fn should_inject_a_reminder_when_the_gate_is_open_and_thresholds_met() {
    // Same history, gate true -> Some(reminder text)
}

#[test]
fn should_not_inject_when_the_gate_is_unset_regardless_of_thresholds() {
    // Default (no gate wired — subagent path / tests) -> injector inert.
    // Rationale: the subagent host does not wire a gate today; the
    // injector must not fire for agents whose tool face is unknown.
}
```

(Use the existing threshold tests in `injection/tests.rs` as scaffolding for the history construction.)

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core turn_flow::injection
```

- [ ] **Step 3: Implement**

In `injection/todo_list.rs`:

```rust
pub struct TodoListReminderInjector {
    todo_list: Arc<TodoList>,
    /// Inject-time gate: is `TodoList` active on the owning agent's tool
    /// face right now? Evaluated on every inject call (US4.2: a mid-session
    /// disable must silence the reminder on the next turn). `None` — the
    /// injector is inert (the wiring site did not supply a tool face).
    tool_gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl TodoListReminderInjector {
    pub fn new(todo_list: Arc<TodoList>) -> Self {
        Self { todo_list, tool_gate: None }
    }

    pub fn with_tool_gate(
        mut self,
        gate: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        self.tool_gate = Some(gate);
        self
    }
}

impl Injector for TodoListReminderInjector {
    fn inject(&self, memory: &ContextMemory) -> Option<String> {
        if let Some(gate) = &self.tool_gate {
            if !gate() {
                return None;
            }
        } else {
            return None;
        }
        // ... existing threshold counting unchanged ...
    }
}
```

**BREAKING with existing tests:** the existing subagent-path tests in `injection/tests.rs` construct the injector bare — they now get `None`. Update them to wire `with_tool_gate(Arc::new(|| true))` (the thresholds under test are unaffected; only the gate is new). This is the correct final-state semantics: no tool face known → no reminder (an agent whose TodoList availability is unknown must not be nagged).

In `turn_flow/mod.rs` extend `set_todo_list` to accept the gate (add `set_todo_tool_gate(gate)` as a separate setter to avoid touching the subagent host call sites; the injector is rebuilt or the gate stored alongside — store `todo_tool_gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>` on the flow, and when assembling injectors at line 1642 apply it if present).

In `message_handlers.rs` (Task 5's site) wire the gate after `set_todo_list`:

```rust
        // US4.2: the reminder only fires while TodoList is actually on the
        // agent's active tool face (re-evaluated per injection).
        let tool_manager = agent.tool_manager();
        turn_flow.set_todo_tool_gate(Arc::new(move || {
            tool_manager.read().map(|tm| tm.is_tool_active("TodoList")).unwrap_or(false)
        }));
```

(Find the real active-set query method name on `ToolManager` — it may be `is_tool_active`, `active_tools().contains`, or similar; grep the tool_registry module. `agent.tool_manager()` returns the `Arc<RwLock<ToolManager>>` handle.)

- [ ] **Step 4: Run tests — pass**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core turn_flow
```

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/turn_flow/injection/todo_list.rs rust/crates/mirri-core/src/turn_flow/mod.rs rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs
git commit -m "feat(todo): gate the stale-todo reminder on TodoList tool activity"
```

---

### Task 7: Compaction summary appendix (G3, F-4)

**Files:**
- Modify: `rust/crates/mirri-core/src/compaction/round.rs:67-88` (`CompactionRoundInput` — add field) and `:422` (the `None` call)
- Modify: the compaction-round call site that owns the session scope (grep `CompactionRoundInput` across mirri-core / mirri-embedded to find the constructor)
- Test: `rust/crates/mirri-core/tests/compaction_tests.rs` (existing `post_process_summary` tests at ~1682)

**Interfaces:**
- Consumes: `post_process_summary(summary: &str, todos: Option<&[String]>) -> String` (existing, `round.rs:570` — the seam already renders `## TODO List`).
- Produces: `CompactionRoundInput.todo_list: Option<&'a Arc<TodoList>>` — `None` in standalone/test rounds.

- [ ] **Step 1: Write the failing test**

In `compaction_tests.rs`:

```rust
#[tokio::test]
async fn should_append_the_todo_list_to_the_compaction_summary_when_present() {
    // Given a mock-LLM round (the file's existing mock harness) and a
    // todo list with one in_progress item, wired into CompactionRoundInput
    // When compaction_round completes
    // Then the result summary contains "## TODO List" and the item title
}

#[tokio::test]
async fn should_not_append_an_empty_todo_section_when_the_list_is_empty() {
    // Given an empty (or None) todo list
    // When compaction_round completes
    // Then the summary contains no "## TODO List" section
}
```

(The end-to-end variant needs the mock-LLM harness; if wiring the full round is disproportionate, test the call-site transformation: given a todo list `[("a", in_progress)]`, the strings passed to `post_process_summary` equal `Some(&[rendered-item])` — follow the existing `post_process_summary` unit tests' rendering format `1. [in_progress] a`? NO — read `round.rs:570-578` first: the function expects pre-rendered item strings and formats them under `## TODO List`. Render each item as `- [status] title` matching the TS `renderTodoList(todos, '## TODO List')` output shape — check TS `fullCompactionService.postProcessSummary` for the exact lines format and copy it.)

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core --test compaction_tests
```

- [ ] **Step 3: Implement**

`CompactionRoundInput` gains:

```rust
    /// The session's todo list — non-empty lists are appended to the
    //  compaction summary (US3.1) so progress survives compaction.
    pub todo_list: Option<&'a crate::todo::TodoList>,
```

At `round.rs:422` replace `None` with:

```rust
    let todo_lines: Vec<String> = input
        .todo_list
        .map(|list| {
            list.get()
                .iter()
                .map(|t| format!("- [{}] {}", t.status.as_str(), t.title))
                .collect()
        })
        .unwrap_or_default();
    let todos = if todo_lines.is_empty() { None } else { Some(todo_lines) };
    let summary = post_process_summary(&summary, todos.as_deref());
```

(Verify the exact rendering the TS side produces — `fullCompactionService.ts:779` uses `renderTodoList(todos, '## TODO List')` whose items are `  [status] title` — and match it character-for-character; adjust the format string above accordingly.)

At the production call site(s), pass `Some(session.todo_list())` / the agent-scope handle.

- [ ] **Step 4: Run tests — pass**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core --test compaction_tests
```

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/compaction/round.rs rust/crates/mirri-core/tests/compaction_tests.rs
git commit -m "feat(todo): append the todo list to compaction summaries"
```

---

### Task 8: Web snapshot preference (G7, F-9)

**Files:**
- Modify: `rust/crates/mirri-server/src/rpc.rs` (`SessionSnapshotResult` / `SnapshotSessionView`, ~line 1924)
- Modify: `apps/mirri-web/src/composables/useMirriWebClient.ts:2153` (todos computed) + possibly `apps/mirri-web/src/api/daemon/ws-rpc-api.ts` (`toAppSnapshot`)
- Test: `apps/mirri-web/src/api/daemon/ws-rpc-models.test.ts` + a composable test if the pattern exists

**Interfaces:**
- Consumes: the folded `ToolsState.store["todo"]` reachable from the snapshot builder's session state; `latestTodos(messages)` (existing derivation).
- Produces: `SnapshotSessionView.todos: Option<Vec<{ title, status }>>` (camelCase `todos` on the wire); web `AppSessionSnapshot` gains `todos`; the composable prefers snapshot todos and falls back to message derivation.

- [ ] **Step 1: Write the failing tests**

In `ws-rpc-models.test.ts` (follow `toAppSnapshot` existing tests):

```typescript
it('maps snapshot todos onto the app snapshot when present', () => {
  // Given an RpcSnapshot whose session carries todos=[{title, status}]
  // When toAppSnapshot
  // Then result.todos equals the mapped array
});

it('keeps todos undefined when the engine omits them', () => {
  // Given a snapshot without todos
  // Then result.todos is undefined (the composable falls back to derivation)
});
```

- [ ] **Step 2: Run to verify failure**

```bash
pnpm --filter mirri-web exec vitest run src/api/daemon/ws-rpc-models.test.ts
```

- [ ] **Step 3: Implement**

Rust side — in `rpc.rs` snapshot builder, populate from the folded state (the snapshot assembly already reads session state for `model`/`busy`; the todo read is `session.todo_list().get()` — the memory cache that restore keeps in sync):

```rust
    /// The session's todo list (the wire-folded state, kept in sync by
    /// restore/undo) — `None`-shaped as an empty vec on the wire (the
    /// consumer treats present-but-empty and absent identically).
    todos: Vec<Value>,
```

with `todos: session.todo_list().get().iter().map(|t| json!({"title": t.title, "status": t.status.as_str()})).collect()` in the builder.

Web side — `toAppSnapshot` maps `snapshot.session.todos` (normalize `done`/`completed` via the existing `toStatus` helper in `latestTodos.ts`); `useMirriWebClient.ts:2153` becomes:

```typescript
const todos = computed<TodoView[]>(() => {
  const sid = rawState.activeSessionId;
  if (!sid) return [];
  const snapshotTodos = rawState.snapshotsBySession[sid]?.todos;
  if (snapshotTodos !== undefined) return snapshotTodos;
  return latestTodos(rawState.messagesBySession[sid] ?? []);
});
```

(Adapt to the actual state store shape — grep `snapshotsBySession` or however snapshots are cached in `rawState`; if snapshots aren't retained per session, retain the last snapshot's todos in a small map updated wherever snapshots land today.)

- [ ] **Step 4: Run tests — pass**

```bash
pnpm --filter mirri-web exec vitest run src/api/daemon/ws-rpc-models.test.ts
pnpm --filter mirri-web exec vitest run src/composables 2>/dev/null || true
```

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-server/src/rpc.rs apps/mirri-web/src/api/daemon/ws-rpc-api.ts apps/mirri-web/src/composables/useMirriWebClient.ts
git commit -m "feat(todo): carry the todo list in session snapshots; web prefers snapshot over derivation"
```

---

### Task 9: Final verification

- [ ] **Step 1: Full rust gate (background, 20+ min)**

```bash
cargo test --manifest-path rust/Cargo.toml --workspace
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
```

- [ ] **Step 2: Web checks**

```bash
pnpm --filter mirri-web exec vue-tsc --noEmit
pnpm --filter mirri-web exec vitest run
```

- [ ] **Step 3: Changeset** — run the `gen-changesets` skill; this is a user-perceivable fix/feature set (todo survives resume/compaction, reminders fire on main agent) → `minor` against `@mirri-ai/release`.

- [ ] **Step 4: Spec status sync** — update `docs/specs/todo-management/todo-management-ROADMAP.md` F-1…F-9 statuses and re-date the context snapshot if code anchors drifted.

- [ ] **Step 5: Commit docs+changeset**

```bash
git add .changeset docs/specs/todo-management
git commit -m "chore(todo): roadmap status sync + changeset"
```

---

## Parallelization

- **Serial chain:** Task 1 → Task 2 → Task 3 → Task 4 (all touch the wire-record/fold/restore spine; 3 and 4 share the `restore.rs` todo block — factor `restore_todo_from_state` in Task 3 and reuse in Task 4).
- **Parallelizable with the chain (separate files, no dependency):** Task 5, Task 7 (reads only the `Arc<TodoList>` API), Task 8 (rust-server snapshot + web files, independent of tasks 1–4's crates).
- **Task 6** touches `turn_flow` and `message_handlers` — parallelizable with tasks 1–4 but conflicts with Task 5's file (`message_handlers.rs`). Run 5 and 6 serially (or as one combined dispatch).
- Recommended schedule: {1→2→3→4} ∥ {7} ∥ {8}, then {5→6}, then 9.

## Self-Review Notes

- Spec coverage: US2.1→T1, US2.2→T2/T3, US2.3→T4, US2.4→T1/T2, US3.1→T7, US3.2→T8, US4.1→T5, US4.2→T6, US4.3→T6 (thresholds frozen, unchanged code paths asserted), US5.1→T3, US5.2→T8. E1 already baseline.
- Known soft spots intentionally left to implementer judgment (flagged in-task): exact test-harness helper names in `session_resume_tests.rs`; `ToolManager`'s active-set query method name; TS compaction rendering line format; web snapshot store shape. Each has a concrete "find it by X" instruction.
- Task 6's gate-`None`-means-inert semantics is a deliberate deviation from "always inject when thresholds met": it matches US4.2 (no known tool face → no nagging) and requires updating existing injection tests — called out in-task.
