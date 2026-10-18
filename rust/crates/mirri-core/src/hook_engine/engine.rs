//! The [`HookEngine`] — manages hook registrations, matching, and parallel
//! execution.
//!
//! The engine maintains an index keyed by [`HookEventType`] for O(1) lookup
//! of candidate hooks, then filters by regex pattern before executing all
//! matches concurrently.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures::future::join_all;

use crate::hook_engine::matcher::matches;
use crate::hook_engine::runner::run_hook;
use crate::hook_engine::types::{
    HookDef, HookEventType, HookOutcome, HookStructuredOutput, HookTriggerResult,
};

/// Hook engine — manages all registered hooks and orchestrates execution.
///
/// The engine is cheap to clone conceptually but is designed to be shared
/// via `&` references (all mutation goes through interior mutability with
/// [`RwLock`]).
pub struct HookEngine {
    /// All registered hooks, in insertion order.
    hooks: RwLock<Vec<HookDef>>,
    /// Index from event type → positions in `hooks`. Rebuilt on every
    /// registration to stay consistent.
    index: RwLock<HashMap<HookEventType, Vec<usize>>>,
    /// Process provider used to execute hook commands (zero-IO T13c-3).
    /// Defaults to [`mirri_os::LocalProcess`]; tests inject scripted fakes
    /// via [`HookEngine::with_process`].
    process: Arc<dyn mirri_os::DynOsProcess>,
}

impl HookEngine {
    /// Create an empty hook engine backed by the host's local process
    /// provider.
    pub fn new() -> Self {
        Self::with_process(Arc::new(mirri_os::LocalProcess::new()))
    }

    /// Create an empty hook engine with an explicit process provider.
    pub fn with_process(process: Arc<dyn mirri_os::DynOsProcess>) -> Self {
        Self {
            hooks: RwLock::new(Vec::new()),
            index: RwLock::new(HashMap::new()),
            process,
        }
    }

    /// Register a single hook.
    pub fn register(&self, hook: HookDef) {
        let mut hooks = self.hooks.write().unwrap_or_else(|e| e.into_inner());
        hooks.push(hook);
        rebuild_index(&hooks, &self.index);
    }

    /// Register multiple hooks at once.
    pub fn register_all(&self, hooks: Vec<HookDef>) {
        let mut guard = self.hooks.write().unwrap_or_else(|e| e.into_inner());
        guard.extend(hooks);
        rebuild_index(&guard, &self.index);
    }

    /// Remove all registered hooks.
    pub fn clear(&self) {
        let mut hooks = self.hooks.write().unwrap_or_else(|e| e.into_inner());
        hooks.clear();
        let mut index = self.index.write().unwrap_or_else(|e| e.into_inner());
        index.clear();
    }

    /// Return a snapshot of all registered hooks.
    pub fn list(&self) -> Vec<HookDef> {
        self.hooks.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Return all hooks registered for a specific event type.
    pub fn list_by_event(&self, event: HookEventType) -> Vec<HookDef> {
        let hooks = self.hooks.read().unwrap_or_else(|e| e.into_inner());
        let index = self.index.read().unwrap_or_else(|e| e.into_inner());
        match index.get(&event) {
            Some(indices) => indices
                .iter()
                .filter_map(|&i| hooks.get(i).cloned())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Trigger all hooks matching `event` and `input`.
    ///
    /// # Execution model
    ///
    /// All matching hooks (pattern matches or pattern is empty) are executed
    /// **concurrently**. The results are merged as follows:
    ///
    /// - If any hook returns `Block`, `blocked` is `true` and `block_reason`
    ///   is set to the first blocking hook's reason.
    /// - `additional_context` is the newline-joined concatenation of all
    ///   passing hooks' `additionalContext`.
    /// - `updated_input` is the last hook's `updatedInput` (in registration
    ///   order) that produced one.
    pub async fn trigger(
        &self,
        event: HookEventType,
        input: serde_json::Value,
        extra_env: &HashMap<String, String>,
    ) -> HookTriggerResult {
        // Collect matching hooks.
        let matching_hooks = {
            let hooks = self.hooks.read().unwrap_or_else(|e| e.into_inner());
            let index = self.index.read().unwrap_or_else(|e| e.into_inner());
            match index.get(&event) {
                Some(indices) => indices
                    .iter()
                    .filter_map(|&i| {
                        let hook = hooks.get(i)?;
                        if matches(&hook.pattern, &input, &event) {
                            Some(hook.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>(),
                None => Vec::new(),
            }
        };

        if matching_hooks.is_empty() {
            return HookTriggerResult::default();
        }

        // Execute all matching hooks concurrently. Each future gets its own
        // clone of the input (serde_json::Value is cheap to clone for typical
        // hook payloads) so they can run in parallel without borrowing.
        let engine_process = Arc::clone(&self.process);
        let futures: Vec<_> = matching_hooks
            .iter()
            .map(|hook| {
                let hook = hook.clone();
                let input_clone = input.clone();
                let extra_env_clone = extra_env.clone();
                let engine_process = Arc::clone(&engine_process);
                async move {
                    let outcome = run_hook(
                        engine_process.as_ref(),
                        &hook,
                        &input_clone,
                        &extra_env_clone,
                    )
                    .await;
                    (hook, outcome)
                }
            })
            .collect();

        let results = join_all(futures).await;

        merge_results(results)
    }
}

impl Default for HookEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for HookEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookEngine")
            .field(
                "hook_count",
                &self.hooks.read().unwrap_or_else(|e| e.into_inner()).len(),
            )
            .finish()
    }
}

/// Rebuild the event-type index from the full hook list.
fn rebuild_index(hooks: &[HookDef], index: &RwLock<HashMap<HookEventType, Vec<usize>>>) {
    let mut new_index: HashMap<HookEventType, Vec<usize>> = HashMap::new();
    for (i, hook) in hooks.iter().enumerate() {
        new_index.entry(hook.event.clone()).or_default().push(i);
    }
    let mut idx = index.write().unwrap_or_else(|e| e.into_inner());
    *idx = new_index;
}

/// Merge per-hook outcomes into a single [`HookTriggerResult`].
///
/// This function is pure and testable in isolation.
pub(crate) fn merge_results(results: Vec<(HookDef, HookOutcome)>) -> HookTriggerResult {
    let mut blocked = false;
    let mut block_reason: Option<String> = None;
    let mut block_message: Option<String> = None;
    let mut additional_contexts: Vec<String> = Vec::new();
    let mut display_messages: Vec<String> = Vec::new();
    let mut updated_input: Option<serde_json::Value> = None;
    let merged_results = results.clone();

    for (_hook, outcome) in &results {
        match outcome {
            HookOutcome::Block {
                reason,
                message,
                additional_context,
                ..
            } => {
                if !blocked {
                    blocked = true;
                    // An empty reason (a silent exit 2, a bare structured
                    // deny) normalises to None so downstream fallbacks
                    // ("Blocked by <event> hook") engage instead of surfacing
                    // an empty deny message.
                    block_reason = (!reason.trim().is_empty()).then(|| reason.trim().to_string());
                    block_message = message
                        .as_deref()
                        .map(str::trim)
                        .filter(|m| !m.is_empty())
                        .map(|m| m.to_string());
                }
                // TS parity: a structured deny's additionalContext rides
                // along with the block (the deny branch keeps it while
                // dropping updatedInput).
                if let Some(ctx) = additional_context.as_deref().filter(|c| !c.is_empty()) {
                    additional_contexts.push(ctx.to_string());
                }
            }
            HookOutcome::Pass(exec_result) => {
                // TS isValidHookResult: timed-out or non-zero-exit passes
                // contribute nothing (neither context nor display message).
                if exec_result.timed_out || exec_result.exit_code.is_some_and(|code| code != 0) {
                    continue;
                }
                // Try to extract structured output for additional context,
                // updated input, and the display message.
                if let Some(structured) = parse_structured_safe(&exec_result.stdout) {
                    // The TS shape nests these under hookSpecificOutput; the
                    // top-level spellings stay accepted.
                    let ctx = structured.additional_context.clone().or_else(|| {
                        structured
                            .hook_specific_output
                            .as_ref()
                            .and_then(|hso| hso.additional_context.clone())
                    });
                    if let Some(ctx) = ctx
                        && !ctx.is_empty()
                    {
                        additional_contexts.push(ctx);
                    }
                    if structured.updated_input.is_some() {
                        updated_input = structured.updated_input.clone();
                    } else if let Some(hso_input) = structured
                        .hook_specific_output
                        .as_ref()
                        .and_then(|hso| hso.updated_input.clone())
                    {
                        updated_input = Some(hso_input);
                    }
                    // TS message chain: top-level message falling back to
                    // hookSpecificOutput.message.
                    if let Some(message) = structured
                        .message
                        .or_else(|| {
                            structured
                                .hook_specific_output
                                .as_ref()
                                .and_then(|hso| hso.message.clone())
                        })
                        .filter(|m| !m.trim().is_empty())
                    {
                        display_messages.push(message);
                    } else {
                        // TS userPromptHookMessage: no structured message —
                        // fall back to the trimmed stdout.
                        let stdout = exec_result.stdout.trim();
                        if !stdout.is_empty() {
                            display_messages.push(stdout.to_string());
                        }
                    }
                } else {
                    // Unstructured stdout: plain-text hook output is the
                    // display message (TS stdout fallback).
                    let stdout = exec_result.stdout.trim();
                    if !stdout.is_empty() {
                        display_messages.push(stdout.to_string());
                    }
                }
            }
            HookOutcome::Error(_) => {
                // Errors are recorded but do not block.
            }
        }
    }

    // Also: TS structured-deny results drop `updatedInput` (the deny branch
    // only carries message/reason/additionalContext) — the rust Block outcome
    // mirrors that: it carries additionalContext but never updatedInput.

    HookTriggerResult {
        blocked,
        block_reason,
        block_message,
        additional_context: additional_contexts.join("\n"),
        message: (!display_messages.is_empty()).then(|| display_messages.join("\n\n")),
        updated_input,
        results: merged_results,
    }
}

/// Best-effort parse of structured output, returning `None` on failure.
fn parse_structured_safe(stdout: &str) -> Option<HookStructuredOutput> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_engine::types::HookBlockSource;

    #[test]
    fn should_return_empty_list_when_listing_empty_engine() {
        let engine = HookEngine::new();
        assert!(engine.list().is_empty());
    }

    #[test]
    fn should_contain_registered_hook_when_listing() {
        let engine = HookEngine::new();
        let hook = HookDef::new(HookEventType::PreToolUse, "echo hello");
        engine.register(hook.clone());

        let list = engine.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].event, HookEventType::PreToolUse);
        assert_eq!(list[0].command, "echo hello");
    }

    #[test]
    fn should_return_only_matching_event_hooks_when_listing_by_event() {
        let engine = HookEngine::new();
        engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
        engine.register(HookDef::new(HookEventType::PostToolUse, "echo b"));
        engine.register(HookDef::new(HookEventType::PreToolUse, "echo c"));

        let pre = engine.list_by_event(HookEventType::PreToolUse);
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0].command, "echo a");
        assert_eq!(pre[1].command, "echo c");

        let post = engine.list_by_event(HookEventType::PostToolUse);
        assert_eq!(post.len(), 1);
        assert_eq!(post[0].command, "echo b");

        let stop = engine.list_by_event(HookEventType::Stop);
        assert!(stop.is_empty());
    }

    #[test]
    fn should_clear_all_hooks_when_clear_is_called() {
        let engine = HookEngine::new();
        engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
        engine.register(HookDef::new(HookEventType::PostToolUse, "echo b"));
        assert_eq!(engine.list().len(), 2);

        engine.clear();
        assert!(engine.list().is_empty());
        assert!(engine.list_by_event(HookEventType::PreToolUse).is_empty());
    }

    #[test]
    fn should_list_all_registered_hooks_when_register_all_is_called() {
        let engine = HookEngine::new();
        engine.register_all(vec![
            HookDef::new(HookEventType::SessionStart, "echo start"),
            HookDef::new(HookEventType::SessionEnd, "echo end"),
        ]);
        assert_eq!(engine.list().len(), 2);
    }

    #[test]
    fn should_join_additional_contexts_when_merging_passing_results() {
        let results = vec![
            (
                HookDef::new(HookEventType::PreToolUse, "echo a"),
                HookOutcome::Pass(make_pass_result(r#"{"additionalContext":"ctx-a"}"#)),
            ),
            (
                HookDef::new(HookEventType::PreToolUse, "echo b"),
                HookOutcome::Pass(make_pass_result(r#"{"additionalContext":"ctx-b"}"#)),
            ),
        ];

        let merged = merge_results(results);
        assert!(!merged.blocked);
        assert_eq!(merged.additional_context, "ctx-a\nctx-b");
    }

    #[test]
    fn should_be_blocked_with_first_reason_when_merging_block_and_pass() {
        let results = vec![
            (
                HookDef::new(HookEventType::PreToolUse, "exit 2"),
                HookOutcome::Block {
                    reason: "Hook exited with code 2".to_string(),
                    message: None,
                    additional_context: None,
                    source: HookBlockSource::ExitCode2,
                },
            ),
            (
                HookDef::new(HookEventType::PreToolUse, "echo a"),
                HookOutcome::Pass(make_pass_result("")),
            ),
        ];

        let merged = merge_results(results);
        assert!(merged.blocked);
        assert_eq!(
            merged.block_reason.as_deref(),
            Some("Hook exited with code 2")
        );
    }

    #[test]
    fn should_use_last_updated_input_when_merging_multiple_updates() {
        let results = vec![
            (
                HookDef::new(HookEventType::PreToolUse, "echo a"),
                HookOutcome::Pass(make_pass_result(r#"{"updatedInput":{"v":1}}"#)),
            ),
            (
                HookDef::new(HookEventType::PreToolUse, "echo b"),
                HookOutcome::Pass(make_pass_result(r#"{"updatedInput":{"v":2}}"#)),
            ),
        ];

        let merged = merge_results(results);
        assert_eq!(merged.updated_input, Some(serde_json::json!({"v": 2})));
    }

    #[test]
    fn should_extract_the_display_message_when_merging_passing_hooks() {
        // Given one hook with a structured message and one emitting plain
        // stdout (the TS message-then-stdout fallback), both valid passes
        let results = vec![
            (
                HookDef::new(HookEventType::UserPromptSubmit, "echo a"),
                HookOutcome::Pass(make_pass_result(r#"{"message":"hello"}"#)),
            ),
            (
                HookDef::new(HookEventType::UserPromptSubmit, "echo b"),
                HookOutcome::Pass(make_pass_result("raw output")),
            ),
        ];

        // When merging
        let merged = merge_results(results);

        // Then the display message joins both with a blank line, mirroring
        // the TS renderUserPromptHookResult displayMessage join
        assert_eq!(merged.message.as_deref(), Some("hello\n\nraw output"));
    }

    #[test]
    fn should_fall_back_to_hook_specific_message_when_no_top_level_message() {
        // Given a passing hook whose message rides under hookSpecificOutput
        let results = vec![(
            HookDef::new(HookEventType::UserPromptSubmit, "echo a"),
            HookOutcome::Pass(make_pass_result(
                r#"{"hookSpecificOutput":{"message":"nested"}}"#,
            )),
        )];

        // When merging
        let merged = merge_results(results);

        // Then the nested message is the display message
        assert_eq!(merged.message.as_deref(), Some("nested"));
    }

    #[test]
    fn should_exclude_failing_hooks_from_the_display_message() {
        // Given a non-zero exit pass and a timed-out pass, both with stdout
        let mut nonzero = make_pass_result("garbage");
        nonzero.exit_code = Some(1);
        let mut timed_out = make_pass_result("late");
        timed_out.timed_out = true;
        let results = vec![
            (
                HookDef::new(HookEventType::UserPromptSubmit, "false"),
                HookOutcome::Pass(nonzero),
            ),
            (
                HookDef::new(HookEventType::UserPromptSubmit, "sleep 5"),
                HookOutcome::Pass(timed_out),
            ),
        ];

        // When merging
        let merged = merge_results(results);

        // Then no display message is extracted (TS isValidHookResult filter)
        assert!(merged.message.is_none());
    }

    #[test]
    fn should_carry_the_block_message_when_merging_a_blocking_hook() {
        // Given a blocking hook carrying a display message
        let results = vec![(
            HookDef::new(HookEventType::UserPromptSubmit, "exit 2"),
            HookOutcome::Block {
                reason: "denied by policy".to_string(),
                message: Some("you shall not pass".to_string()),
                additional_context: None,
                source: HookBlockSource::ExitCode2,
            },
        )];

        // When merging
        let merged = merge_results(results);

        // Then the block message survives alongside the reason
        assert!(merged.blocked);
        assert_eq!(merged.block_reason.as_deref(), Some("denied by policy"));
        assert_eq!(merged.block_message.as_deref(), Some("you shall not pass"));
    }

    #[test]
    fn should_accept_nested_additional_context_when_merging_passing_hooks() {
        // Given a passing hook whose additionalContext rides under
        // hookSpecificOutput (the TS shape)
        let results = vec![(
            HookDef::new(HookEventType::UserPromptSubmit, "echo a"),
            HookOutcome::Pass(make_pass_result(
                r#"{"hookSpecificOutput":{"additionalContext":"nested ctx"}}"#,
            )),
        )];

        // When merging
        let merged = merge_results(results);

        // Then the nested context is collected
        assert_eq!(merged.additional_context, "nested ctx");
    }

    #[test]
    fn should_collect_additional_context_when_merging_a_structured_deny() {
        // Given a structured-deny block carrying additionalContext alongside
        // a passing hook with its own context
        let results = vec![
            (
                HookDef::new(HookEventType::PreToolUse, "deny"),
                HookOutcome::Block {
                    reason: "no".to_string(),
                    message: None,
                    additional_context: Some("deny-ctx".to_string()),
                    source: HookBlockSource::StructuredOutput,
                },
            ),
            (
                HookDef::new(HookEventType::PreToolUse, "echo a"),
                HookOutcome::Pass(make_pass_result(r#"{"additionalContext":"pass-ctx"}"#)),
            ),
        ];

        // When merging
        let merged = merge_results(results);

        // Then both contexts are collected — TS `includeBlock` parity — and
        // the block still carries its reason.
        assert!(merged.blocked);
        assert_eq!(merged.block_reason.as_deref(), Some("no"));
        assert_eq!(merged.additional_context, "deny-ctx\npass-ctx");
    }

    fn make_pass_result(stdout: &str) -> crate::hook_engine::types::HookExecutionResult {
        crate::hook_engine::types::HookExecutionResult {
            exit_code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
            timed_out: false,
        }
    }
}
