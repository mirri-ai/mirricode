//! Integration tests for the permission gate and policy chain.

use std::collections::HashSet;
use std::sync::Arc;

use mirri_core::cancel::CancelToken;
use mirri_core::permission::{
    ParsedPattern, PermissionManager, PermissionMode, PermissionPolicyContext,
    PermissionPolicyResult, PermissionRule, RuleDecision, RuleScope, match_permission_rule,
    parse_pattern,
};
use mirri_core::tool::{AccessKind, AccessOperation, ToolAccess, ToolInputDisplay};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a [`PermissionPolicyContext`] from the given parameters and evaluate
/// the policy chain via a fresh [`PermissionManager`].
///
/// This mirrors how the `TurnFlow` would build the context: it pulls
/// `effective_rules` from the manager but takes `session_approvals` directly
/// (in production these come from `all_session_approvals()`).
fn eval(
    mode: PermissionMode,
    rules: &[PermissionRule],
    session_approvals: &HashSet<String>,
    tool_name: &str,
    display: &ToolInputDisplay,
    accesses: &[ToolAccess],
) -> PermissionPolicyResult {
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let manager = PermissionManager::new(mode, rules.to_vec());
    let effective_rules = manager.effective_rules();
    let ctx = PermissionPolicyContext {
        tool_name,
        tool_call_id: "test-call",
        args: &args,
        approval_rule: "",
        display,
        accesses,
        mode,
        rules: &effective_rules,
        session_approvals,
        cancel: &cancel,
        net_class: None,
    };
    manager.evaluate(&ctx)
}

fn empty_approvals() -> HashSet<String> {
    HashSet::new()
}

fn approvals(items: &[&str]) -> HashSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn rule(decision: RuleDecision, pattern: &str) -> PermissionRule {
    PermissionRule {
        decision,
        scope: RuleScope::User,
        pattern: pattern.to_string(),
        reason: None,
    }
}

fn file_write_access(path: &str) -> ToolAccess {
    ToolAccess {
        kind: AccessKind::File,
        path: Some(path.to_string()),
        operation: AccessOperation::Write,
    }
}

fn generic_display() -> ToolInputDisplay {
    ToolInputDisplay::Generic {
        summary: "test".to_string(),
    }
}

// ---------------------------------------------------------------------------
// 1. Policy chain order: deny before auto-approve
// ---------------------------------------------------------------------------

#[test]
fn should_deny_when_deny_rule_matches_in_auto_mode() {
    let rules = [rule(RuleDecision::Deny, "Bash")];
    let result = eval(
        PermissionMode::Auto,
        &rules,
        &empty_approvals(),
        "Bash",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "deny rule should beat auto-approve, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 2. Policy chain order: deny before yolo
// ---------------------------------------------------------------------------

#[test]
fn should_deny_when_deny_rule_matches_in_yolo_mode() {
    let rules = [rule(RuleDecision::Deny, "Write")];
    let result = eval(
        PermissionMode::Yolo,
        &rules,
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "deny rule should beat yolo, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 3. Auto mode approve
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_auto_mode_with_no_matching_rules() {
    let result = eval(
        PermissionMode::Auto,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "auto mode should approve, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 4. Yolo mode approve
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_yolo_mode() {
    let result = eval(
        PermissionMode::Yolo,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "yolo mode should approve, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 5. Manual mode fallback ask
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_manual_mode_with_no_matching_rules() {
    let result = eval(
        PermissionMode::Manual,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "manual mode with no matching rules should fall back to ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 6. Default approve tools
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_tool_is_default_approved_in_manual_mode() {
    for tool_name in &["Read", "Grep", "Glob"] {
        let result = eval(
            PermissionMode::Manual,
            &[],
            &empty_approvals(),
            tool_name,
            &generic_display(),
            &[],
        );
        assert!(
            matches!(result, PermissionPolicyResult::Approve { .. }),
            "{} should be default-approved in manual mode, got {:?}",
            tool_name,
            result
        );
    }
}

// ---------------------------------------------------------------------------
// 7. User deny rule
// ---------------------------------------------------------------------------

#[test]
fn should_deny_when_user_deny_rule_matches() {
    let rules = [rule(RuleDecision::Deny, "Bash")];
    let result = eval(
        PermissionMode::Manual,
        &rules,
        &empty_approvals(),
        "Bash",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "deny rule should deny, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 8. User allow rule
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_user_allow_rule_matches() {
    let rules = [rule(RuleDecision::Allow, "Read")];
    let result = eval(
        PermissionMode::Manual,
        &rules,
        &empty_approvals(),
        "Read",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "allow rule should approve, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 9. User ask rule
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_user_ask_rule_matches() {
    let rules = [rule(RuleDecision::Ask, "Write")];
    let result = eval(
        PermissionMode::Manual,
        &rules,
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "ask rule should ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 10. Session approval beats ask rule
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_session_approval_exists_for_ask_rule() {
    let rules = [rule(RuleDecision::Ask, "Bash")];
    let session = approvals(&["Bash"]);
    let result = eval(
        PermissionMode::Manual,
        &rules,
        &session,
        "Bash",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "session approval should beat ask rule, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 11. Sensitive file access → ask
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_writing_to_sensitive_file() {
    let accesses = [file_write_access(".env")];
    let result = eval(
        PermissionMode::Manual,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &accesses,
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "writing to .env should ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 12. Git control path access → ask
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_writing_to_git_control_path() {
    let accesses = [file_write_access(".git/config")];
    let result = eval(
        PermissionMode::Manual,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &accesses,
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "writing to .git/config should ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 13. Auto mode + AskUserQuestion → deny
// ---------------------------------------------------------------------------

#[test]
fn should_deny_when_ask_user_question_in_auto_mode() {
    let result = eval(
        PermissionMode::Auto,
        &[],
        &empty_approvals(),
        "AskUserQuestion",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "auto mode should deny AskUserQuestion, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 14. ExitPlanMode review → ask (manual mode)
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_exit_plan_mode_review_in_manual_mode() {
    let display = ToolInputDisplay::PlanReview {
        plan: "plan".to_string(),
        path: None,
        options: Vec::new(),
    };
    let result = eval(
        PermissionMode::Manual,
        &[],
        &empty_approvals(),
        "ExitPlanMode",
        &display,
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "ExitPlanMode review in manual mode should ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 15. ExitPlanMode auto mode → approve (auto-approve runs first)
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_exit_plan_mode_in_auto_mode() {
    let display = ToolInputDisplay::PlanReview {
        plan: "plan".to_string(),
        path: None,
        options: Vec::new(),
    };
    let result = eval(
        PermissionMode::Auto,
        &[],
        &empty_approvals(),
        "ExitPlanMode",
        &display,
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "ExitPlanMode in auto mode should be approved by auto-approve policy, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 16. Parse pattern: Write(/foo/**)
// ---------------------------------------------------------------------------

#[test]
fn should_parse_tool_name_and_arg_pattern_when_pattern_has_argument() {
    let p = parse_pattern("Write(/foo/**)").unwrap();
    assert_eq!(p.tool_name, "Write");
    assert_eq!(p.arg_pattern.as_deref(), Some("/foo/**"));
}

// ---------------------------------------------------------------------------
// 17. Parse pattern: bare tool name
// ---------------------------------------------------------------------------

#[test]
fn should_parse_tool_name_without_arg_pattern_when_pattern_is_bare() {
    let p = parse_pattern("Bash").unwrap();
    assert_eq!(p.tool_name, "Bash");
    assert_eq!(p.arg_pattern, None);
}

// ---------------------------------------------------------------------------
// 18. Parse pattern: empty parens
// ---------------------------------------------------------------------------

#[test]
fn should_parse_tool_name_without_arg_pattern_when_pattern_has_empty_parens() {
    let p: ParsedPattern = parse_pattern("Tool()").unwrap();
    assert_eq!(p.tool_name, "Tool");
    assert_eq!(p.arg_pattern, None);
}

// ---------------------------------------------------------------------------
// 19. Match rule: tool name glob
// ---------------------------------------------------------------------------

#[test]
fn should_match_when_glob_pattern_matches_tool_name() {
    assert!(match_permission_rule(
        "mcp__github__*",
        "mcp__github__create_pr",
        None
    ));
}

// ---------------------------------------------------------------------------
// 20. Match rule: exact tool name
// ---------------------------------------------------------------------------

#[test]
fn should_match_when_pattern_exactly_matches_tool_name() {
    assert!(match_permission_rule("Read", "Read", None));
}

// ---------------------------------------------------------------------------
// 21. Match rule: no match
// ---------------------------------------------------------------------------

#[test]
fn should_not_match_when_pattern_does_not_match_tool_name() {
    assert!(!match_permission_rule("Write", "Read", None));
}

// ---------------------------------------------------------------------------
// 22. Subagent inheritance: effective_rules includes parent's rules
// ---------------------------------------------------------------------------

#[test]
fn should_inherit_parent_rules_when_subagent_created_with_parent() {
    let parent_rules = [rule(RuleDecision::Allow, "Bash")];
    let parent = PermissionManager::new(PermissionMode::Manual, parent_rules.to_vec());
    let parent_arc = Arc::new(parent);
    let child = PermissionManager::with_parent(PermissionMode::Manual, vec![], parent_arc);
    let effective = child.effective_rules();
    assert!(
        effective
            .iter()
            .any(|r| r.pattern == "Bash" && r.decision == RuleDecision::Allow),
        "child's effective_rules should include parent's Bash allow rule, got {:?}",
        effective
    );
}

// ---------------------------------------------------------------------------
// 23. Session approval inheritance: child sees parent's approvals
// ---------------------------------------------------------------------------

#[test]
fn should_inherit_parent_session_approvals_when_subagent_created_with_parent() {
    let parent = PermissionManager::new(PermissionMode::Manual, vec![]);
    parent.record_approval("Bash".to_string());
    let parent_arc = Arc::new(parent);
    let child = PermissionManager::with_parent(PermissionMode::Manual, vec![], parent_arc);
    let approvals = child.all_session_approvals();
    assert!(
        approvals.contains(&"Bash".to_string()),
        "child's all_session_approvals should include parent's 'Bash' approval, got {:?}",
        approvals
    );
}

// ---------------------------------------------------------------------------
// 24. Runtime rule swap: a swapped-in allow rule approves what used to ask
// ---------------------------------------------------------------------------

/// Given a manual-mode chain with a runtime-swapped allow rule, when a
/// previously-asked tool is called again, then the session-approval
/// history policy is bypassed by the user-configured allow policy.
///
/// (The live-manager transition itself — `evaluate` observing a swapped set
/// on the SAME manager — is pinned by the in-module test
/// `should_evaluate_new_decision_when_set_rules_swaps_rules_at_runtime`
/// in `permission/mod.rs`; this integration test pins the chain-level
/// effect of the extracted rule set.)
#[test]
fn should_approve_when_allow_rule_swapped_in_at_runtime() {
    let manager = PermissionManager::new(PermissionMode::Manual, Vec::new());
    let display = generic_display();
    assert!(matches!(
        eval(
            PermissionMode::Manual,
            &[],
            &empty_approvals(),
            "Bash",
            &display,
            &[]
        ),
        PermissionPolicyResult::Ask { .. }
    ));

    manager.set_rules(vec![rule(RuleDecision::Allow, "Bash")]);
    let effective = manager.effective_rules();
    let result = eval(
        PermissionMode::Manual,
        &effective,
        &empty_approvals(),
        "Bash",
        &display,
        &[],
    );
    assert!(matches!(result, PermissionPolicyResult::Approve { .. }));
}
