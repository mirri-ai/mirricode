//! The 21 default permission policies.
//!
//! Each policy is a zero-sized struct implementing [`PermissionPolicy`] (or
//! holds an `Arc` to a service it consults). The policies are evaluated in
//! order by [`PermissionManager::evaluate`](super::PermissionManager::evaluate);
//! the first non-[`Pass`](PermissionPolicyResult::Pass) result wins.
//!
//! No hook policy lives in this chain (v2 parity): the `PreToolUse` external
//! hook fires once at the tool-executor boundary
//! ([`TurnFlowHooks::prepare_tool_execution`](crate::turn_flow::TurnFlowHooks)),
//! and `PermissionRequest` / `PermissionResult` are observation-only events.
//!
//! ## Chain order (highest priority first)
//!
//! 1.  `AgentSwarmExclusiveDenyPolicy` — swarm mode: deny the `Agent` tool.
//! 2.  `AutoModeAskUserQuestionDenyPolicy` — auto mode + AskUserQuestion → deny.
//! 3.  `PlanModeGuardDenyPolicy` — plan mode: deny everything except the
//!     read-only / plan-control allow-list.
//! 4.  `UserConfiguredDenyPolicy` — user deny rule → deny.
//! 5.  `PrivateNetworkFetchDenyPolicy` — FetchURL to a cloud metadata
//!     endpoint → deny (ADR-010).
//! 6.  `PrivateNetworkFetchAskPolicy` — FetchURL to a private-network
//!     target → ask (ADR-010; fires in every mode).
//! 7.  `AutoModeApprovePolicy` — auto mode → approve.
//! 8.  `SessionApprovalHistoryPolicy` — "approve for session" memory → approve.
//! 9.  `UserConfiguredAskPolicy` — user ask rule → ask.
//! 10. `UserConfiguredAllowPolicy` — user allow rule → approve.
//! 11. `ExitPlanModeReviewAskPolicy` — ExitPlanMode review → ask.
//! 12. `GoalStartReviewAskPolicy` — CreateGoal start → ask.
//! 13. `PlanModeToolApprovePolicy` — EnterPlanMode/ExitPlanMode → approve.
//! 14. `SensitiveFileAccessAskPolicy` — sensitive file access → ask.
//! 15. `GitControlPathAccessAskPolicy` — `.git/` access → ask.
//! 16. `YoloModeApprovePolicy` — yolo mode → approve.
//! 17. `SwarmModeAgentSwarmApprovePolicy` — swarm mode + AgentSwarm → ask.
//! 18. `WorkspaceBoundaryPolicy` — outside-sandbox paths → ask / `path:` rules.
//! 19. `DefaultToolApprovePolicy` — default-approve tool list → approve.
//! 20. `GitCwdWriteApprovePolicy` — write to `.git/` → ask.
//! 21. `FallbackAskPolicy` — fallback → ask.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use globset::Glob;

use crate::plan::PlanManager;
use crate::swarm::SwarmModeService;
use crate::tool::{AccessKind, AccessOperation, ToolInputDisplay};
use crate::tools::{PathClassification, WorkspacePaths};

use super::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Appended to every denied/cancelled outside-sandbox authorization outcome.
/// Hard contract (plan §2): sandbox/permission messages must never point the
/// model at an alternative access route — the only legitimate path is asking
/// the user.
pub const OUTSIDE_SANDBOX_DENIAL_CONTRACT: &str = "NEVER attempt to access this path \
    through any other means (other tools, shell commands, sub-agents, or alternate path \
    spellings). If the task genuinely requires it, ask the user directly and respect \
    their decision.";

/// Tools that are approved by default (read-only / safe tools).
const DEFAULT_APPROVE_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "ReadMediaFile",
    "TodoList",
    "TaskList",
    "TaskOutput",
    "CronList",
    "WebSearch",
    "FetchURL",
    "Agent",
    "AskUserQuestion",
    "Skill",
    "GetGoal",
    "SetGoalBudget",
    "UpdateGoal",
    "select_tools",
];

/// Check whether a file path points to a sensitive file.
///
/// Sensitive files include:
/// - `.env` files (except `.env.example`, `.env.sample`, `.env.template`).
/// - SSH private keys (`id_rsa`, `id_ed25519`, `id_ecdsa` without `.pub`).
/// - Files containing "credentials" in the name.
/// - `.aws/credentials` and `.gcp/credentials` paths.
fn is_sensitive_file(path: &str) -> bool {
    let path = path.to_lowercase();
    let basename = path.rsplit('/').next().unwrap_or(&path);

    // .env files (except .env.example/.sample/.template)
    if basename.starts_with(".env") {
        return !basename.ends_with(".example")
            && !basename.ends_with(".sample")
            && !basename.ends_with(".template");
    }

    // SSH private keys
    if basename.starts_with("id_rsa")
        || basename.starts_with("id_ed25519")
        || basename.starts_with("id_ecdsa")
    {
        return !basename.ends_with(".pub");
    }

    // Credentials files
    if basename.contains("credentials") {
        return true;
    }

    // .aws/credentials, .gcp/credentials
    if path.contains(".aws/credentials") || path.contains(".gcp/credentials") {
        return true;
    }

    false
}

/// Check whether a file path is inside a `.git` control directory.
fn is_git_control_path(path: &str) -> bool {
    path.split('/').any(|component| component == ".git")
}

// ---------------------------------------------------------------------------
// Policy 1: swarm mode denies subagent spawning
// ---------------------------------------------------------------------------

/// Deny the `Agent` tool (subagent spawning) while swarm mode is active.
///
/// In swarm mode all subagent work must go through `AgentSwarm`; a bare
/// `Agent` spawn would bypass the swarm-batch coordination, so it is denied.
///
/// The TypeScript original also enforces `AgentSwarm` batch exclusivity
/// (one `AgentSwarm` call per model response, alone in its batch). That
/// check needs the whole batch, which the per-call
/// [`PermissionPolicyContext`] does not carry — it is enforced at the batch
/// level, not here.
pub struct AgentSwarmExclusiveDenyPolicy {
    swarm_mode: Option<Arc<Mutex<SwarmModeService>>>,
}

impl AgentSwarmExclusiveDenyPolicy {
    /// Create the policy. Pass `None` when no swarm-mode state is available —
    /// the policy then always passes.
    pub fn new(swarm_mode: Option<Arc<Mutex<SwarmModeService>>>) -> Self {
        Self { swarm_mode }
    }
}

impl PermissionPolicy for AgentSwarmExclusiveDenyPolicy {
    fn name(&self) -> &str {
        "agent-swarm-exclusive-deny"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        let Some(swarm_mode) = &self.swarm_mode else {
            return PermissionPolicyResult::Pass;
        };
        let swarm_mode = swarm_mode.lock().unwrap_or_else(|e| e.into_inner());
        if !swarm_mode.is_active() {
            return PermissionPolicyResult::Pass;
        }
        if ctx.tool_name.eq_ignore_ascii_case("agent") {
            return PermissionPolicyResult::Deny {
                reason: Some("agent-swarm-exclusive-deny".into()),
                message: Some(
                    "Swarm mode is active: the Agent tool (subagent spawning) is not allowed. \
                     Use AgentSwarm to run subagents in swarm mode."
                        .into(),
                ),
            };
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 3: auto mode + AskUserQuestion → deny
// ---------------------------------------------------------------------------

pub struct AutoModeAskUserQuestionDenyPolicy;
impl PermissionPolicy for AutoModeAskUserQuestionDenyPolicy {
    fn name(&self) -> &str {
        "auto-mode-ask-user-question-deny"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        if ctx.mode == PermissionMode::Auto && ctx.tool_name == "AskUserQuestion" {
            return PermissionPolicyResult::Deny {
                reason: Some("auto-mode-ask-user-question".into()),
                message: Some("AskUserQuestion is not allowed in auto mode".into()),
            };
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 4: plan mode guard → deny
// ---------------------------------------------------------------------------

/// Tools permitted while plan mode is active.
///
/// Plan mode is read-only: the agent may inspect the workspace (`read`,
/// `glob`, `grep`), ask the user clarifying questions (`AskUserQuestion` —
/// the injected plan-mode reminders instruct the model to use it), and
/// manage plan mode itself (`EnterPlanMode`/`ExitPlanMode`). Everything
/// else — writes, executions, scheduling, sub-agents — is denied until
/// the plan is submitted via `ExitPlanMode`.
///
/// The list carries both spellings because tool names are compared with
/// `eq_ignore_ascii_case`, which bridges casing (`read`↔`Read`) but not
/// underscores (`exit_plan_mode`↔`ExitPlanMode`) — and the rust builtins
/// are PascalCase. The snake_case entries stay for wire/protocol callers
/// that use the TS spelling.
const PLAN_MODE_ALLOWED_TOOLS: &[&str] = &[
    "enter_plan_mode",
    "exit_plan_mode",
    "EnterPlanMode",
    "ExitPlanMode",
    "AskUserQuestion",
    "read",
    "glob",
    "grep",
];

/// Deny every tool outside the plan-mode allow-list while plan mode is
/// active.
pub struct PlanModeGuardDenyPolicy {
    plan: Option<Arc<PlanManager>>,
}

impl PlanModeGuardDenyPolicy {
    /// Create the policy. Pass `None` when no plan manager is available —
    /// the policy then always passes.
    pub fn new(plan: Option<Arc<PlanManager>>) -> Self {
        Self { plan }
    }
}

impl PermissionPolicy for PlanModeGuardDenyPolicy {
    fn name(&self) -> &str {
        "plan-mode-guard-deny"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        let Some(plan) = &self.plan else {
            return PermissionPolicyResult::Pass;
        };
        if !plan.is_active() {
            return PermissionPolicyResult::Pass;
        }
        if PLAN_MODE_ALLOWED_TOOLS
            .iter()
            .any(|allowed| ctx.tool_name.eq_ignore_ascii_case(allowed))
        {
            return PermissionPolicyResult::Pass;
        }
        PermissionPolicyResult::Deny {
            reason: Some("plan-mode-guard-deny".into()),
            message: Some(format!(
                "Plan mode is active. Only read-only tools (Read, Grep, Glob), AskUserQuestion, \
                 and plan-mode controls (EnterPlanMode, ExitPlanMode) are allowed. '{tool}' is not \
                 permitted while in plan mode.",
                tool = ctx.tool_name,
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Policy 5: user deny rule matches → deny
// ---------------------------------------------------------------------------

pub struct UserConfiguredDenyPolicy;
impl PermissionPolicy for UserConfiguredDenyPolicy {
    fn name(&self) -> &str {
        "user-configured-deny"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        let user_scopes = [RuleScope::TurnOverride, RuleScope::Project, RuleScope::User];
        for rule in ctx.rules.iter() {
            if rule.decision != RuleDecision::Deny {
                continue;
            }
            if !user_scopes.contains(&rule.scope) {
                continue;
            }
            if match_permission_rule(&rule.pattern, ctx.tool_name, None) {
                return PermissionPolicyResult::Deny {
                    reason: rule.reason.clone(),
                    message: Some(format!("Denied by rule: {}", rule.pattern)),
                };
            }
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 5: cloud-metadata FetchURL → deny (ADR-010)
// ---------------------------------------------------------------------------

/// Deny FetchURL calls targeting cloud metadata endpoints (ADR-010).
///
/// The metadata endpoints (`169.254.169.254`, `fd00:ec2::254`) expose
/// cloud credentials to anything that can reach them; a model "approval"
/// of such a fetch is almost certainly induced by prompt injection, so
/// no approval path exists. The denial carries the NEVER-bypass
/// behavioral contract.
pub struct PrivateNetworkFetchDenyPolicy;

impl PermissionPolicy for PrivateNetworkFetchDenyPolicy {
    fn name(&self) -> &str {
        "private-network-fetch-deny"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        if ctx.tool_name != "FetchURL" {
            return PermissionPolicyResult::Pass;
        }
        let Some((crate::tools::net_class::NetClass::Metadata, ips)) = &ctx.net_class else {
            return PermissionPolicyResult::Pass;
        };
        PermissionPolicyResult::Deny {
            reason: Some("cloud-metadata-endpoint".into()),
            message: Some(format!(
                "Refusing to fetch cloud metadata endpoint \"{ip}\" — permanently blocked to protect credentials. \
                 NEVER attempt to reach it through any other means (other tools, shell commands, sub-agents, \
                 or alternate spellings). If the task genuinely requires it, ask the user directly and respect \
                 their decision.",
                ip = ips.first().map(|i| i.to_string()).unwrap_or_default()
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Policy 6: private-network FetchURL → ask (ADR-010)
// ---------------------------------------------------------------------------

/// Ask for approval on private-network FetchURL targets (ADR-010).
///
/// Fires in EVERY permission mode (it sits ahead of the auto/yolo
/// approve policies): a private-address fetch has ambiguous intent
/// (model-induced SSRF or a genuine user delegation), so the user — not
/// the mode — decides.
///
/// Exemption self-check (ADR-010 decision 3): before asking, the policy
/// consults the session-approval memory and user allow rules for the
/// call's host facet (`FetchURL(host:<host>)`, port-insensitive). On a
/// hit it Passes, and the chain-level approval comes from
/// `DefaultToolApprovePolicy` (`FetchURL` is in `DEFAULT_APPROVE_TOOLS`)
/// — NOT from `SessionApprovalHistoryPolicy`/`UserConfiguredAllowPolicy`,
/// whose `match_permission_rule(..., None)` never matches parameterized
/// patterns. `FetchURL` must therefore stay in `DEFAULT_APPROVE_TOOLS`.
///
/// Allow-rule matching is a literal host comparison, not a glob: a
/// pattern like `FetchURL(host:*.internal)` does NOT match (glob support
/// for host facets is future work). User deny rules (bare `FetchURL`)
/// still win — they sit at chain position 4, ahead of this policy. User
/// ask rules for the facet are shadowed by this ask (same terminal
/// outcome); parameterized ask rules don't match at all today
/// (pre-existing DSL limitation).
pub struct PrivateNetworkFetchAskPolicy;

/// Extract the host from a `FetchURL(host:<host>)` facet string; when the
/// approval rule is not that shape, fall back to the `url` argument.
fn fetch_host_from_ctx(ctx: &PermissionPolicyContext) -> Option<String> {
    if let Some(host) = ctx
        .approval_rule
        .strip_prefix("FetchURL(host:")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return Some(host.to_string());
    }
    ctx.args
        .get("url")
        .and_then(|value| value.as_str())
        .and_then(crate::tools::net_class::host_of_url)
}

/// First three IP addresses joined by commas, ` (+N)` when more than three.
fn summarize_ips(ips: &[std::net::IpAddr]) -> String {
    let head = ips
        .iter()
        .take(3)
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if ips.len() > 3 {
        format!("{head} (+{})", ips.len() - 3)
    } else {
        head
    }
}

impl PermissionPolicy for PrivateNetworkFetchAskPolicy {
    fn name(&self) -> &str {
        "private-network-fetch-ask"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        if ctx.tool_name != "FetchURL" {
            return PermissionPolicyResult::Pass;
        }
        let Some((crate::tools::net_class::NetClass::Askable, ips)) = &ctx.net_class else {
            return PermissionPolicyResult::Pass;
        };
        let Some(host) = fetch_host_from_ctx(ctx) else {
            return PermissionPolicyResult::Pass;
        };

        // Exemption self-check (ADR-010 decision 3): the session-approval
        // memory or a user allow rule covering the call's host facet
        // suppresses the ask.
        if ctx.session_approvals.contains(ctx.approval_rule)
            || ctx
                .session_approvals
                .contains(&format!("FetchURL(host:{host})"))
        {
            return PermissionPolicyResult::Pass;
        }
        let user_scopes = [RuleScope::TurnOverride, RuleScope::Project, RuleScope::User];
        let facet_hit = |rule: &PermissionRule| {
            rule.decision == RuleDecision::Allow
                && user_scopes.contains(&rule.scope)
                && match_permission_rule(
                    &rule.pattern,
                    "FetchURL",
                    Some(&|arg: &str| arg == format!("host:{host}")),
                )
        };
        if ctx.rules.iter().any(facet_hit) {
            return PermissionPolicyResult::Pass;
        }

        // Report the segment of the address that triggered the ask (the
        // first Askable one), not the first resolved IP — the first may be
        // public while a later one is private. Metadata never reaches here
        // (the deny policy ahead of it intercepts it), so Askable is the
        // strictest class in this list.
        let trigger_ip = ips
            .iter()
            .find(|ip| {
                crate::tools::net_class::classify_ip(ip)
                    == crate::tools::net_class::NetClass::Askable
            })
            .or_else(|| ips.first());
        let class_name = match trigger_ip {
            Some(ip) => crate::tools::net_class::segment_name(ip),
            None => "private",
        };
        PermissionPolicyResult::Ask {
            reason: Some(format!(
                "private-network fetch: host '{host}' resolves to {} ({class_name})",
                summarize_ips(ips)
            )),
            grant_dir: None,
            deny_when_unattended: true,
        }
    }
}

pub struct AutoModeApprovePolicy;
impl PermissionPolicy for AutoModeApprovePolicy {
    fn name(&self) -> &str {
        "auto-mode-approve"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        if ctx.mode == PermissionMode::Auto {
            return PermissionPolicyResult::Approve {
                reason: Some("auto-mode".into()),
            };
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 7: session-approval memory matches → approve
// ---------------------------------------------------------------------------

pub struct SessionApprovalHistoryPolicy;
impl PermissionPolicy for SessionApprovalHistoryPolicy {
    fn name(&self) -> &str {
        "session-approval-history"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        for pattern in ctx.session_approvals {
            if match_permission_rule(pattern, ctx.tool_name, None) {
                return PermissionPolicyResult::Approve {
                    reason: Some("approve for session".into()),
                };
            }
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 8: user ask rule matches → ask
// ---------------------------------------------------------------------------

pub struct UserConfiguredAskPolicy;
impl PermissionPolicy for UserConfiguredAskPolicy {
    fn name(&self) -> &str {
        "user-configured-ask"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        let user_scopes = [RuleScope::TurnOverride, RuleScope::Project, RuleScope::User];
        for rule in ctx.rules.iter() {
            if rule.decision != RuleDecision::Ask {
                continue;
            }
            if !user_scopes.contains(&rule.scope) {
                continue;
            }
            if match_permission_rule(&rule.pattern, ctx.tool_name, None) {
                return PermissionPolicyResult::Ask {
                    reason: rule
                        .reason
                        .clone()
                        .or_else(|| Some(format!("Ask rule: {}", rule.pattern))),
                    grant_dir: None,
                    deny_when_unattended: false,
                };
            }
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 9: user allow rule matches → approve
// ---------------------------------------------------------------------------

pub struct UserConfiguredAllowPolicy;
impl PermissionPolicy for UserConfiguredAllowPolicy {
    fn name(&self) -> &str {
        "user-configured-allow"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        let user_scopes = [RuleScope::TurnOverride, RuleScope::Project, RuleScope::User];
        for rule in ctx.rules.iter() {
            if rule.decision != RuleDecision::Allow {
                continue;
            }
            if !user_scopes.contains(&rule.scope) {
                continue;
            }
            if match_permission_rule(&rule.pattern, ctx.tool_name, None) {
                return PermissionPolicyResult::Approve {
                    reason: rule.reason.clone(),
                };
            }
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 10: ExitPlanMode review → ask
// ---------------------------------------------------------------------------

pub struct ExitPlanModeReviewAskPolicy;
impl PermissionPolicy for ExitPlanModeReviewAskPolicy {
    fn name(&self) -> &str {
        "exit-plan-mode-review-ask"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        if ctx.tool_name == "ExitPlanMode"
            && let ToolInputDisplay::PlanReview { .. } = ctx.display
            && ctx.mode != PermissionMode::Auto
        {
            return PermissionPolicyResult::Ask {
                reason: Some("plan-review".into()),
                grant_dir: None,
                deny_when_unattended: false,
            };
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 11: CreateGoal start → ask
// ---------------------------------------------------------------------------

pub struct GoalStartReviewAskPolicy;
impl PermissionPolicy for GoalStartReviewAskPolicy {
    fn name(&self) -> &str {
        "goal-start-review-ask"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        if ctx.tool_name == "CreateGoal"
            && let ToolInputDisplay::GoalStart { .. } = ctx.display
            && ctx.mode != PermissionMode::Auto
        {
            return PermissionPolicyResult::Ask {
                reason: Some("goal-start".into()),
                grant_dir: None,
                deny_when_unattended: false,
            };
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 12: plan-mode tools → approve
// ---------------------------------------------------------------------------

pub struct PlanModeToolApprovePolicy;
impl PermissionPolicy for PlanModeToolApprovePolicy {
    fn name(&self) -> &str {
        "plan-mode-tool-approve"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        if ctx.tool_name == "EnterPlanMode" || ctx.tool_name == "ExitPlanMode" {
            return PermissionPolicyResult::Approve {
                reason: Some("plan-mode-tool".into()),
            };
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 13: sensitive file → ask
// ---------------------------------------------------------------------------

pub struct SensitiveFileAccessAskPolicy;
impl PermissionPolicy for SensitiveFileAccessAskPolicy {
    fn name(&self) -> &str {
        "sensitive-file-access-ask"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        for access in ctx.accesses {
            if access.kind == AccessKind::File
                && let Some(path) = &access.path
                && is_sensitive_file(path)
            {
                return PermissionPolicyResult::Ask {
                    reason: Some("sensitive-file".into()),
                    grant_dir: None,
                    deny_when_unattended: false,
                };
            }
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 14: git control path → ask
// ---------------------------------------------------------------------------

pub struct GitControlPathAccessAskPolicy;
impl PermissionPolicy for GitControlPathAccessAskPolicy {
    fn name(&self) -> &str {
        "git-control-path-access-ask"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        for access in ctx.accesses {
            if access.kind == AccessKind::File
                && let Some(path) = &access.path
                && is_git_control_path(path)
            {
                return PermissionPolicyResult::Ask {
                    reason: Some("git-control-path".into()),
                    grant_dir: None,
                    deny_when_unattended: false,
                };
            }
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 15: yolo mode → approve
// ---------------------------------------------------------------------------

pub struct YoloModeApprovePolicy;
impl PermissionPolicy for YoloModeApprovePolicy {
    fn name(&self) -> &str {
        "yolo-mode-approve"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        if ctx.mode == PermissionMode::Yolo {
            return PermissionPolicyResult::Approve {
                reason: Some("yolo-mode".into()),
            };
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 16: swarm mode + AgentSwarm → ask
// ---------------------------------------------------------------------------

/// Require approval for the `AgentSwarm` tool while swarm mode is active.
///
/// Swarm batches can fan out to many agents and consume significant
/// resources, so they are not default-approved: the user is asked before a
/// swarm run starts.
///
/// Both spellings of the tool name are recognized: `AgentSwarm` (PascalCase,
/// used by the TypeScript agent-core) and `agent-swarm` (snake-case).
pub struct SwarmModeAgentSwarmApprovePolicy {
    swarm_mode: Option<Arc<Mutex<SwarmModeService>>>,
}

impl SwarmModeAgentSwarmApprovePolicy {
    /// Create the policy. Pass `None` when no swarm-mode state is available —
    /// the policy then always passes.
    pub fn new(swarm_mode: Option<Arc<Mutex<SwarmModeService>>>) -> Self {
        Self { swarm_mode }
    }
}

impl PermissionPolicy for SwarmModeAgentSwarmApprovePolicy {
    fn name(&self) -> &str {
        "swarm-mode-agent-swarm-approve"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        let Some(swarm_mode) = &self.swarm_mode else {
            return PermissionPolicyResult::Pass;
        };
        let swarm_mode = swarm_mode.lock().unwrap_or_else(|e| e.into_inner());
        if !swarm_mode.is_active() {
            return PermissionPolicyResult::Pass;
        }
        let tool_name = ctx.tool_name.to_ascii_lowercase();
        if tool_name == "agent-swarm" || tool_name == "agentswarm" {
            return PermissionPolicyResult::Ask {
                reason: Some("swarm-mode-agent-swarm".into()),
                grant_dir: None,
                deny_when_unattended: false,
            };
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 16: workspace boundary — outside-sandbox paths → ask / path: rules
// ---------------------------------------------------------------------------

/// The file and search tools whose explicit `path` argument falls under the
/// workspace sandbox boundary (plan D1 — the same six tools `classify_tool_path`
/// migrates).
const BOUNDARY_TOOLS: &[&str] = &["Read", "Write", "Edit", "Glob", "Grep", "ReadMediaFile"];

/// Route clean outside-sandbox paths through the permission chain.
///
/// The file/search tools (Task 1) no longer refuse an outside absolute path
/// themselves — the request reaches the chain and this policy decides where
/// it sits relative to the session sandbox (plan D2).
///
/// Evaluation:
/// - `services.workspace_paths` not wired → Pass (legacy hosts keep their
///   pre-policy behavior).
/// - Non-boundary tool, or no non-empty string `args["path"]` → Pass.
///   `extract_file_accesses` is deliberately untouched, so
///   `SensitiveFileAccessAskPolicy` / `GitControlPathAccessAskPolicy` keep
///   their existing Glob/Grep behavior.
/// - [`WorkspacePaths::classify`] → `Inside` / `Escape` → Pass (Inside falls
///   to the regular policies; Escape is already refused at the tool layer).
/// - `Outside(canonical)` → the first `path:` rule whose glob matches the
///   canonical path wins: deny → deny, allow → approve, ask → ask with the
///   rule reason. No matching rule → ask with the outside reason. Both asks
///   carry the `grant_dir` Task 3 turns into once/session/persist directory
///   grants (the canonical path itself for search roots, its parent for file
///   targets).
///
/// The mode split needs no code here: `AutoModeApprovePolicy` (chain
/// position 5) and `YoloModeApprovePolicy` (chain position 14) sit ahead, so
/// this policy effectively only fires in Manual mode — the mode semantics are
/// the user contract (pinned by chain tests in `permission/mod.rs`).
pub struct WorkspaceBoundaryPolicy {
    workspace_paths: Option<Arc<WorkspacePaths>>,
}

impl WorkspaceBoundaryPolicy {
    /// Create the policy. Pass `None` when no sandbox is wired — the policy
    /// then always passes.
    pub fn new(workspace_paths: Option<Arc<WorkspacePaths>>) -> Self {
        Self { workspace_paths }
    }
}

impl PermissionPolicy for WorkspaceBoundaryPolicy {
    fn name(&self) -> &str {
        "workspace-boundary"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        let Some(workspace_paths) = &self.workspace_paths else {
            return PermissionPolicyResult::Pass;
        };
        if !BOUNDARY_TOOLS.contains(&ctx.tool_name) {
            return PermissionPolicyResult::Pass;
        }
        let Some(path_arg) = ctx.args.get("path").and_then(|value| value.as_str()) else {
            return PermissionPolicyResult::Pass;
        };
        if path_arg.is_empty() {
            return PermissionPolicyResult::Pass;
        }

        let PathClassification::Outside(canonical) = workspace_paths.classify(Path::new(path_arg))
        else {
            // Inside → the regular policies decide (session approvals,
            // default-approve, ...); Escape → refused at the tool layer
            // before the chain ever runs.
            return PermissionPolicyResult::Pass;
        };

        // `path:` rules, first match wins. Only rules carrying the `path:`
        // prefix are interpreted here — the tool-name DSL
        // (`match_permission_rule`) is a dead end and stays untouched (plan
        // D2). A malformed glob never matches (the rule is skipped).
        let matched = ctx.rules.iter().find(|rule| {
            let Some(suffix) = rule.pattern.strip_prefix("path:") else {
                return false;
            };
            Glob::new(suffix)
                .map(|glob| glob.compile_matcher().is_match(&canonical))
                .unwrap_or(false)
        });

        let grant_dir = grant_dir_for(ctx.tool_name, &canonical);
        match matched {
            Some(rule) if rule.decision == RuleDecision::Deny => PermissionPolicyResult::Deny {
                reason: Some("outside-sandbox path rule".into()),
                // Plan D4: every outside-sandbox denial carries the
                // NEVER-bypass contract — the model must not route around
                // the declined rule through other tools or spellings.
                message: Some(format!(
                    "access to paths matching '{}' is denied by rule. \
                     {OUTSIDE_SANDBOX_DENIAL_CONTRACT}",
                    rule.pattern
                )),
            },
            Some(rule) if rule.decision == RuleDecision::Allow => PermissionPolicyResult::Approve {
                reason: Some("outside-sandbox path rule".into()),
            },
            Some(_) => PermissionPolicyResult::Ask {
                reason: Some("outside-sandbox path rule".into()),
                grant_dir: Some(grant_dir),
                deny_when_unattended: false,
            },
            None => PermissionPolicyResult::Ask {
                reason: Some("the path is outside the workspace sandbox".into()),
                grant_dir: Some(grant_dir),
                deny_when_unattended: false,
            },
        }
    }
}

/// The directory a positive approval would authorize (no I/O, by tool
/// category): the canonical path itself when it is a search root (Glob/Grep
/// take a directory argument), otherwise its parent — a file target never
/// grants the file, always the enclosing directory. A canonical path with no
/// parent (`/`) degenerates to itself.
fn grant_dir_for(tool_name: &str, canonical: &Path) -> PathBuf {
    if matches!(tool_name, "Glob" | "Grep") {
        return canonical.to_path_buf();
    }
    canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| canonical.to_path_buf())
}

// ---------------------------------------------------------------------------
// Policy 17: default-approve tools → approve
// ---------------------------------------------------------------------------

pub struct DefaultToolApprovePolicy;
impl PermissionPolicy for DefaultToolApprovePolicy {
    fn name(&self) -> &str {
        "default-tool-approve"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        if DEFAULT_APPROVE_TOOLS.contains(&ctx.tool_name) {
            return PermissionPolicyResult::Approve {
                reason: Some("default-approve".into()),
            };
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 18: git cwd write → ask
// ---------------------------------------------------------------------------

/// Ask for approval when a tool call writes into the `.git` control
/// directory.
///
/// Writes inside `.git/` (objects, refs, hooks, config) mutate repository
/// internals and can corrupt or overwrite VCS state, so they require
/// explicit user approval. Read accesses are deliberately not gated here —
/// they are handled by [`GitControlPathAccessAskPolicy`].
///
/// Note: `is_git_control_path` matches any path component named `.git`,
/// which also covers `path.contains(".git/")` and paths that end in `.git`.
pub struct GitCwdWriteApprovePolicy;

impl PermissionPolicy for GitCwdWriteApprovePolicy {
    fn name(&self) -> &str {
        "git-cwd-write-approve"
    }
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        for access in ctx.accesses {
            let writes = matches!(
                access.operation,
                AccessOperation::Write | AccessOperation::ReadWrite
            );
            if !writes {
                continue;
            }
            if let Some(path) = &access.path
                && is_git_control_path(path)
            {
                return PermissionPolicyResult::Ask {
                    reason: Some("git-cwd-write".into()),
                    grant_dir: None,
                    deny_when_unattended: false,
                };
            }
        }
        PermissionPolicyResult::Pass
    }
}

// ---------------------------------------------------------------------------
// Policy 19: fallback → ask
// ---------------------------------------------------------------------------

pub struct FallbackAskPolicy;
impl PermissionPolicy for FallbackAskPolicy {
    fn name(&self) -> &str {
        "fallback-ask"
    }
    fn evaluate(&self, _ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        PermissionPolicyResult::Ask {
            reason: Some("fallback".into()),
            grant_dir: None,
            deny_when_unattended: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use crate::cancel::CancelToken;
    use crate::swarm::SwarmModeTrigger;
    use crate::tool::ToolAccess;

    use super::*;

    #[test]
    fn should_treat_env_files_as_sensitive_when_path_is_env_file() {
        assert!(is_sensitive_file(".env"));
        assert!(is_sensitive_file("/home/user/.env"));
        assert!(!is_sensitive_file(".env.example"));
        assert!(!is_sensitive_file(".env.sample"));
        assert!(!is_sensitive_file(".env.template"));
    }

    #[test]
    fn should_detect_ssh_private_keys_as_sensitive_when_path_is_private_key() {
        assert!(is_sensitive_file("id_rsa"));
        assert!(is_sensitive_file("/home/user/.ssh/id_rsa"));
        assert!(is_sensitive_file("id_ed25519"));
        assert!(!is_sensitive_file("id_rsa.pub"));
        assert!(!is_sensitive_file("id_ed25519.pub"));
    }

    #[test]
    fn should_detect_credentials_files_as_sensitive_when_path_contains_credentials() {
        assert!(is_sensitive_file("credentials.json"));
        assert!(is_sensitive_file("/root/.aws/credentials"));
        assert!(is_sensitive_file("/root/.gcp/credentials"));
    }

    #[test]
    fn should_not_flag_regular_files_as_sensitive_when_path_is_regular_file() {
        assert!(!is_sensitive_file("src/main.rs"));
        assert!(!is_sensitive_file("README.md"));
        assert!(!is_sensitive_file("package.json"));
    }

    #[test]
    fn should_detect_git_control_path_when_path_contains_dot_git_component() {
        assert!(is_git_control_path(".git/config"));
        assert!(is_git_control_path("/repo/.git/HEAD"));
        assert!(is_git_control_path("repo/.git/hooks/pre-commit"));
        assert!(!is_git_control_path("src/main.rs"));
        assert!(!is_git_control_path(".github/workflows/ci.yml"));
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Evaluate a policy against a minimal Manual-mode context.
    fn evaluate(
        policy: &dyn PermissionPolicy,
        tool_name: &str,
        accesses: &[ToolAccess],
    ) -> PermissionPolicyResult {
        evaluate_with(policy, tool_name, accesses, &serde_json::Value::Null)
    }

    /// Evaluate a policy with explicit tool-call arguments.
    fn evaluate_with(
        policy: &dyn PermissionPolicy,
        tool_name: &str,
        accesses: &[ToolAccess],
        args: &serde_json::Value,
    ) -> PermissionPolicyResult {
        let cancel = CancelToken::new();
        let display = ToolInputDisplay::Generic {
            summary: "test".into(),
        };
        let ctx = PermissionPolicyContext {
            tool_name,
            tool_call_id: "call-1",
            args,
            approval_rule: "",
            display: &display,
            accesses,
            mode: PermissionMode::Manual,
            rules: &[],
            session_approvals: &HashSet::new(),
            cancel: &cancel,
            net_class: None,
        };
        policy.evaluate(&ctx)
    }

    fn file_access(path: &str, operation: AccessOperation) -> ToolAccess {
        ToolAccess {
            kind: AccessKind::File,
            path: Some(path.to_string()),
            operation,
        }
    }

    /// Evaluate a fetch policy against a full FetchURL context: args carry
    /// the `url`, the approval rule is the matching host facet, and the
    /// net classification / session approvals / user rules are explicit.
    fn fetch_eval(
        policy: &dyn PermissionPolicy,
        url: &str,
        net_class: Option<(crate::tools::net_class::NetClass, Vec<std::net::IpAddr>)>,
        approvals: &[&str],
        rules: &[PermissionRule],
    ) -> PermissionPolicyResult {
        let cancel = CancelToken::new();
        let display = ToolInputDisplay::Generic {
            summary: "test".into(),
        };
        let args = serde_json::json!({ "url": url });
        let approval_rule = crate::tools::net_class::host_of_url(url)
            .map(|host| crate::tools::net_class::fetch_host_facet(&host))
            .unwrap_or_else(|| "FetchURL".to_string());
        let approvals: HashSet<String> = approvals.iter().map(|s| s.to_string()).collect();
        let ctx = PermissionPolicyContext {
            tool_name: "FetchURL",
            tool_call_id: "call-1",
            args: &args,
            approval_rule: &approval_rule,
            display: &display,
            accesses: &[],
            mode: PermissionMode::Manual,
            rules,
            session_approvals: &approvals,
            cancel: &cancel,
            net_class,
        };
        policy.evaluate(&ctx)
    }

    fn fetch_ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    // -----------------------------------------------------------------------
    // PrivateNetworkFetchDenyPolicy (ADR-010)
    // -----------------------------------------------------------------------

    /// Given a FetchURL call whose target resolves to the cloud metadata
    /// endpoint, when the deny policy evaluates it, then it denies with the
    /// metadata reason and a NEVER-bypass message naming the endpoint.
    #[test]
    fn should_deny_when_fetch_target_is_cloud_metadata() {
        let policy = PrivateNetworkFetchDenyPolicy;
        let result = fetch_eval(
            &policy,
            "http://169.254.169.254/latest/meta-data/",
            Some((
                crate::tools::net_class::NetClass::Metadata,
                vec![fetch_ip("169.254.169.254")],
            )),
            &[],
            &[],
        );
        match result {
            PermissionPolicyResult::Deny { reason, message } => {
                assert_eq!(reason.as_deref(), Some("cloud-metadata-endpoint"));
                let message = message.expect("a deny carries a user-visible message");
                assert!(
                    message.contains("cloud metadata endpoint"),
                    "message: {message}"
                );
                assert!(message.contains("169.254.169.254"), "message: {message}");
                assert!(message.contains("NEVER"), "message: {message}");
            }
            other => panic!("a metadata fetch must deny, got {other:?}"),
        }
    }

    /// Given a FetchURL call whose target is not a metadata endpoint, when
    /// the deny policy evaluates it, then it passes (public and askable
    /// targets are not this policy's business).
    #[test]
    fn should_pass_when_fetch_target_not_metadata() {
        let policy = PrivateNetworkFetchDenyPolicy;
        for (url, class, ip) in [
            (
                "http://93.184.216.34/",
                crate::tools::net_class::NetClass::Public,
                "93.184.216.34",
            ),
            (
                "http://10.0.0.1/x",
                crate::tools::net_class::NetClass::Askable,
                "10.0.0.1",
            ),
        ] {
            let result = fetch_eval(&policy, url, Some((class, vec![fetch_ip(ip)])), &[], &[]);
            assert!(
                matches!(result, PermissionPolicyResult::Pass),
                "expected Pass for {url}, got {result:?}"
            );
        }
    }

    /// Given a FetchURL call whose target resolves to a private address,
    /// when the ask policy evaluates it, then it asks with an unattended
    /// denial marker and a reason naming the host and its segment.
    #[test]
    fn should_ask_when_fetch_target_askable() {
        let policy = PrivateNetworkFetchAskPolicy;
        let result = fetch_eval(
            &policy,
            "http://10.0.0.1/x",
            Some((
                crate::tools::net_class::NetClass::Askable,
                vec![fetch_ip("10.0.0.1")],
            )),
            &[],
            &[],
        );
        match result {
            PermissionPolicyResult::Ask {
                reason,
                deny_when_unattended,
                ..
            } => {
                assert!(
                    deny_when_unattended,
                    "a private ask must deny when unattended"
                );
                let reason = reason.expect("a private ask carries a reason");
                assert!(reason.contains("10.0.0.1"), "reason: {reason}");
                assert!(reason.contains("RFC1918"), "reason: {reason}");
            }
            other => panic!("an askable fetch must ask, got {other:?}"),
        }
    }

    /// Given a FetchURL call whose host resolves to a public address first
    /// and a private address second, when the ask policy evaluates it, then
    /// the reason names the segment of the address that triggered the ask
    /// (RFC1918), not the first resolved IP's segment (public).
    #[test]
    fn should_report_triggering_segment_name_when_first_ip_is_public() {
        let policy = PrivateNetworkFetchAskPolicy;
        let result = fetch_eval(
            &policy,
            "http://dns.test/",
            Some((
                crate::tools::net_class::NetClass::Askable,
                vec![fetch_ip("8.8.8.8"), fetch_ip("10.0.0.1")],
            )),
            &[],
            &[],
        );
        match result {
            PermissionPolicyResult::Ask { reason, .. } => {
                let reason = reason.expect("a private ask carries a reason");
                assert!(reason.contains("RFC1918"), "reason: {reason}");
                assert!(!reason.contains("public"), "reason: {reason}");
            }
            other => panic!("an askable fetch must ask, got {other:?}"),
        }
    }

    /// Given a session-approval memory entry covering the fetch host facet,
    /// when the ask policy evaluates the same host, then it passes — the
    /// user already approved this host for the session.
    #[test]
    fn should_pass_when_session_memory_covers_host() {
        let policy = PrivateNetworkFetchAskPolicy;
        let result = fetch_eval(
            &policy,
            "http://10.0.0.1/x",
            Some((
                crate::tools::net_class::NetClass::Askable,
                vec![fetch_ip("10.0.0.1")],
            )),
            &["FetchURL(host:10.0.0.1)"],
            &[],
        );
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    /// Given a user allow rule covering the fetch host facet, when the ask
    /// policy evaluates the host, then it passes — the user's standing
    /// allow replaces the ask.
    #[test]
    fn should_pass_when_user_allow_rule_covers_host() {
        let policy = PrivateNetworkFetchAskPolicy;
        let rules = vec![PermissionRule {
            decision: RuleDecision::Allow,
            scope: RuleScope::User,
            pattern: "FetchURL(host:10.0.0.1)".to_string(),
            reason: None,
        }];
        let result = fetch_eval(
            &policy,
            "http://10.0.0.1/x",
            Some((
                crate::tools::net_class::NetClass::Askable,
                vec![fetch_ip("10.0.0.1")],
            )),
            &[],
            &rules,
        );
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    /// Given a user allow rule whose host facet names a different host, when
    /// the ask policy evaluates a private fetch to another host, then it
    /// still asks — an allow for `other.host` never covers `10.0.0.1`.
    #[test]
    fn should_ask_when_allow_rule_host_does_not_match() {
        let policy = PrivateNetworkFetchAskPolicy;
        let rules = vec![PermissionRule {
            decision: RuleDecision::Allow,
            scope: RuleScope::User,
            pattern: "FetchURL(host:other.host)".to_string(),
            reason: None,
        }];
        let result = fetch_eval(
            &policy,
            "http://10.0.0.1/x",
            Some((
                crate::tools::net_class::NetClass::Askable,
                vec![fetch_ip("10.0.0.1")],
            )),
            &[],
            &rules,
        );
        assert!(
            matches!(result, PermissionPolicyResult::Ask { .. }),
            "an allow rule for a different host must not cover this target, \
             got {result:?}"
        );
    }

    /// Given a user allow rule using a glob pattern in the host facet, when
    /// the ask policy evaluates a matching-by-name private fetch, then it
    /// still asks — host facets match literally, glob support is future
    /// work, so an unapproved host stays behind the ask gate.
    #[test]
    fn should_ask_when_allow_rule_uses_glob_pattern() {
        let policy = PrivateNetworkFetchAskPolicy;
        let rules = vec![PermissionRule {
            decision: RuleDecision::Allow,
            scope: RuleScope::User,
            pattern: "FetchURL(host:*.internal)".to_string(),
            reason: None,
        }];
        let result = fetch_eval(
            &policy,
            "http://host.internal/x",
            Some((
                crate::tools::net_class::NetClass::Askable,
                vec![fetch_ip("10.0.0.1")],
            )),
            &[],
            &rules,
        );
        assert!(
            matches!(result, PermissionPolicyResult::Ask { .. }),
            "a glob allow rule must not match host facets literally, got \
             {result:?}"
        );
    }

    /// Given the private-network fetch approval flow, when
    /// `DEFAULT_APPROVE_TOOLS` is checked, then `FetchURL` must be present —
    /// the exemption pass-through (ADR-010) relies on the host-facet
    /// approval path, which only fires when the tool is approvable.
    #[test]
    fn should_keep_fetchurl_in_default_approve_tools_when_exemption_pass_through_depends_on_it() {
        assert!(
            DEFAULT_APPROVE_TOOLS.contains(&"FetchURL"),
            "FetchURL must stay in DEFAULT_APPROVE_TOOLS: the ADR-010 \
             private-network exemption pass-through depends on the host-facet \
             approval path it enables"
        );
    }

    /// Given a FetchURL call with no pre-resolved net classification (DNS
    /// failed or another tool), when the ask policy evaluates it, then it
    /// passes — the provider-layer enforcement still refuses private
    /// targets, so the chain stays out of the way.
    #[test]
    fn should_pass_when_net_class_absent() {
        for policy in [
            &PrivateNetworkFetchDenyPolicy as &dyn PermissionPolicy,
            &PrivateNetworkFetchAskPolicy,
        ] {
            let result = fetch_eval(policy, "http://example.invalid/", None, &[], &[]);
            assert!(
                matches!(result, PermissionPolicyResult::Pass),
                "expected Pass without net_class, got {result:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // PlanModeGuardDenyPolicy
    // -----------------------------------------------------------------------

    #[test]
    fn should_pass_when_no_plan_manager() {
        let policy = PlanModeGuardDenyPolicy::new(None);
        let result = evaluate(&policy, "Write", &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_plan_mode_is_inactive() {
        let policy = PlanModeGuardDenyPolicy::new(Some(Arc::new(PlanManager::new())));
        let result = evaluate(&policy, "Write", &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_plan_mode_active_and_tool_is_allow_listed() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Explore and report".to_string()).unwrap();
        let policy = PlanModeGuardDenyPolicy::new(Some(plan));
        for tool in ["read", "glob", "grep", "enter_plan_mode", "exit_plan_mode"] {
            let result = evaluate(&policy, tool, &[]);
            assert!(
                matches!(result, PermissionPolicyResult::Pass),
                "expected Pass for '{tool}', got {result:?}"
            );
        }
    }

    #[test]
    fn should_pass_when_plan_mode_active_and_read_tool_uses_pascal_case() {
        // Rust builtin tools use PascalCase names; the allow-list is snake_case.
        let plan = Arc::new(PlanManager::new());
        plan.enter("Explore".to_string()).unwrap();
        let policy = PlanModeGuardDenyPolicy::new(Some(plan));
        let result = evaluate(&policy, "Read", &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_plan_mode_active_and_plan_controls_use_pascal_case() {
        // The rust builtins are PascalCase; the allow-list must recognize
        // both spellings or plan mode becomes a one-way door.
        let plan = Arc::new(PlanManager::new());
        plan.enter("Explore".to_string()).unwrap();
        let policy = PlanModeGuardDenyPolicy::new(Some(plan));
        for tool in ["EnterPlanMode", "ExitPlanMode", "AskUserQuestion"] {
            let result = evaluate(&policy, tool, &[]);
            assert!(
                matches!(result, PermissionPolicyResult::Pass),
                "expected Pass for '{tool}', got {result:?}"
            );
        }
    }

    #[test]
    fn should_deny_when_plan_mode_active_and_tool_is_mutating() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Explore".to_string()).unwrap();
        let policy = PlanModeGuardDenyPolicy::new(Some(plan));
        for tool in ["Write", "Edit", "Bash", "TaskStop", "CronCreate", "Agent"] {
            let result = evaluate(&policy, tool, &[]);
            assert!(
                matches!(result, PermissionPolicyResult::Deny { .. }),
                "expected Deny for '{tool}', got {result:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // AgentSwarmExclusiveDenyPolicy
    // -----------------------------------------------------------------------

    #[test]
    fn should_pass_when_agent_tool_evaluated_without_swarm_state() {
        let policy = AgentSwarmExclusiveDenyPolicy::new(None);
        let result = evaluate(&policy, "Agent", &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_swarm_mode_inactive_and_agent_tool_called() {
        let policy =
            AgentSwarmExclusiveDenyPolicy::new(Some(Arc::new(Mutex::new(SwarmModeService::new()))));
        let result = evaluate(&policy, "Agent", &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_deny_when_swarm_mode_active_and_agent_tool_called() {
        let swarm_mode = Arc::new(Mutex::new(SwarmModeService::new()));
        swarm_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enter(SwarmModeTrigger::Manual);
        let policy = AgentSwarmExclusiveDenyPolicy::new(Some(swarm_mode));
        let result = evaluate(&policy, "Agent", &[]);
        assert!(matches!(result, PermissionPolicyResult::Deny { .. }));
    }

    #[test]
    fn should_pass_when_swarm_mode_active_and_non_agent_tool_called() {
        let swarm_mode = Arc::new(Mutex::new(SwarmModeService::new()));
        swarm_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enter(SwarmModeTrigger::Manual);
        let policy = AgentSwarmExclusiveDenyPolicy::new(Some(swarm_mode));
        let result = evaluate(&policy, "Read", &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    // -----------------------------------------------------------------------
    // SwarmModeAgentSwarmApprovePolicy
    // -----------------------------------------------------------------------

    #[test]
    fn should_pass_when_swarm_mode_inactive_and_agent_swarm_tool_called() {
        let policy = SwarmModeAgentSwarmApprovePolicy::new(Some(Arc::new(Mutex::new(
            SwarmModeService::new(),
        ))));
        let result = evaluate(&policy, "AgentSwarm", &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_ask_when_swarm_mode_active_and_agent_swarm_tool_called() {
        let swarm_mode = Arc::new(Mutex::new(SwarmModeService::new()));
        swarm_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enter(SwarmModeTrigger::Manual);
        let policy = SwarmModeAgentSwarmApprovePolicy::new(Some(swarm_mode));
        // Both the PascalCase (`AgentSwarm`) and snake-case (`agent-swarm`)
        // spellings must be recognized.
        for tool in ["AgentSwarm", "agent-swarm"] {
            let result = evaluate(&policy, tool, &[]);
            assert!(
                matches!(result, PermissionPolicyResult::Ask { .. }),
                "expected Ask for '{tool}', got {result:?}"
            );
        }
    }

    #[test]
    fn should_pass_when_swarm_mode_active_and_non_swarm_tool_called() {
        let swarm_mode = Arc::new(Mutex::new(SwarmModeService::new()));
        swarm_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enter(SwarmModeTrigger::Manual);
        let policy = SwarmModeAgentSwarmApprovePolicy::new(Some(swarm_mode));
        let result = evaluate(&policy, "Read", &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_toggle_active_state_when_swarm_mode_entered_and_exited() {
        let mut state = SwarmModeService::new();
        assert!(!state.is_active());
        state.enter(SwarmModeTrigger::Manual);
        assert!(state.is_active());
        state.exit();
        assert!(!state.is_active());
    }

    // -----------------------------------------------------------------------
    // GitCwdWriteApprovePolicy
    // -----------------------------------------------------------------------

    #[test]
    fn should_ask_when_write_access_targets_git_dir() {
        let policy = GitCwdWriteApprovePolicy;
        let accesses = vec![file_access(".git/config", AccessOperation::Write)];
        let result = evaluate(&policy, "Write", &accesses);
        assert!(matches!(result, PermissionPolicyResult::Ask { .. }));
    }

    #[test]
    fn should_ask_when_read_write_access_targets_git_dir() {
        let policy = GitCwdWriteApprovePolicy;
        let accesses = vec![file_access("/repo/.git/HEAD", AccessOperation::ReadWrite)];
        let result = evaluate(&policy, "Edit", &accesses);
        assert!(matches!(result, PermissionPolicyResult::Ask { .. }));
    }

    #[test]
    fn should_pass_when_read_access_targets_git_dir() {
        // Reads of .git are gated by GitControlPathAccessAskPolicy instead.
        let policy = GitCwdWriteApprovePolicy;
        let accesses = vec![file_access(".git/config", AccessOperation::Read)];
        let result = evaluate(&policy, "Read", &accesses);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_write_access_targets_outside_git_dir() {
        let policy = GitCwdWriteApprovePolicy;
        let accesses = vec![file_access("/repo/src/main.rs", AccessOperation::Write)];
        let result = evaluate(&policy, "Write", &accesses);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    // -----------------------------------------------------------------------
    // Manager wiring
    // -----------------------------------------------------------------------

    #[test]
    fn should_deny_when_manager_evaluates_write_with_plan_mode_active() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Plan".to_string()).unwrap();
        let manager = PermissionManager::with_services(
            PermissionMode::Manual,
            Vec::new(),
            PermissionServices {
                plan: Some(plan),
                ..Default::default()
            },
        );

        let cancel = CancelToken::new();
        let display = ToolInputDisplay::Generic {
            summary: "test".into(),
        };
        let args = serde_json::Value::Null;
        let ctx = PermissionPolicyContext {
            tool_name: "Write",
            tool_call_id: "call-1",
            args: &args,
            approval_rule: "",
            display: &display,
            accesses: &[],
            mode: PermissionMode::Manual,
            rules: &[],
            session_approvals: &HashSet::new(),
            cancel: &cancel,
            net_class: None,
        };
        assert!(matches!(
            manager.evaluate(&ctx),
            PermissionPolicyResult::Deny { .. }
        ));
    }

    #[test]
    fn should_ask_for_plan_review_when_manager_evaluates_exit_plan_mode_with_plan_mode_active() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Step 1: do the thing".to_string()).unwrap();
        let manager = PermissionManager::with_services(
            PermissionMode::Yolo,
            Vec::new(),
            PermissionServices {
                plan: Some(plan),
                ..Default::default()
            },
        );
        let display = ToolInputDisplay::PlanReview {
            plan: "Step 1: do the thing".to_string(),
            path: None,
            options: Vec::new(),
        };
        let cancel = CancelToken::new();
        let ctx = PermissionPolicyContext {
            tool_name: "ExitPlanMode",
            tool_call_id: "call-1",
            args: &serde_json::Value::Null,
            approval_rule: "ExitPlanMode",
            display: &display,
            accesses: &[],
            mode: PermissionMode::Yolo,
            rules: &[],
            session_approvals: &std::collections::HashSet::new(),
            cancel: &cancel,
            net_class: None,
        };

        match manager.evaluate(&ctx) {
            PermissionPolicyResult::Ask { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("plan-review"));
            }
            other => panic!("expected Ask for ExitPlanMode under active plan mode, got {other:?}"),
        }
    }

    #[test]
    fn should_approve_when_manager_evaluates_exit_plan_mode_in_auto_mode_with_plan_mode_active() {
        // Auto mode intentionally skips the plan review (ADR-005/auto
        // semantics: auto-approve runs at chain position 5, ahead of the
        // review-ask at 9) — this test locks that contract.
        let plan = Arc::new(PlanManager::new());
        plan.enter("Step 1: do the thing".to_string()).unwrap();
        let manager = PermissionManager::with_services(
            PermissionMode::Auto,
            Vec::new(),
            PermissionServices {
                plan: Some(plan),
                ..Default::default()
            },
        );
        let display = ToolInputDisplay::PlanReview {
            plan: "Step 1: do the thing".to_string(),
            path: None,
            options: Vec::new(),
        };
        let cancel = CancelToken::new();
        let ctx = PermissionPolicyContext {
            tool_name: "ExitPlanMode",
            tool_call_id: "call-1",
            args: &serde_json::Value::Null,
            approval_rule: "ExitPlanMode",
            display: &display,
            accesses: &[],
            mode: PermissionMode::Auto,
            rules: &[],
            session_approvals: &std::collections::HashSet::new(),
            cancel: &cancel,
            net_class: None,
        };

        match manager.evaluate(&ctx) {
            PermissionPolicyResult::Approve { .. } => {}
            other => panic!("expected Approve for ExitPlanMode in auto mode, got {other:?}"),
        }
    }

    #[test]
    fn should_approve_when_manager_evaluates_read_with_plan_mode_active() {
        let plan = Arc::new(PlanManager::new());
        plan.enter("Plan".to_string()).unwrap();
        let manager = PermissionManager::with_services(
            PermissionMode::Manual,
            Vec::new(),
            PermissionServices {
                plan: Some(plan),
                ..Default::default()
            },
        );

        let cancel = CancelToken::new();
        let display = ToolInputDisplay::Generic {
            summary: "test".into(),
        };
        let args = serde_json::Value::Null;
        let ctx = PermissionPolicyContext {
            tool_name: "Read",
            tool_call_id: "call-1",
            args: &args,
            approval_rule: "",
            display: &display,
            accesses: &[],
            mode: PermissionMode::Manual,
            rules: &[],
            session_approvals: &HashSet::new(),
            cancel: &cancel,
            net_class: None,
        };
        // The plan-mode guard passes for read tools; DefaultToolApprovePolicy
        // then approves.
        assert!(matches!(
            manager.evaluate(&ctx),
            PermissionPolicyResult::Approve { .. }
        ));
    }

    #[test]
    fn should_deny_when_manager_evaluates_agent_with_swarm_mode_active() {
        let swarm_mode = Arc::new(Mutex::new(SwarmModeService::new()));
        swarm_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enter(SwarmModeTrigger::Manual);
        let manager = PermissionManager::with_services(
            PermissionMode::Manual,
            Vec::new(),
            PermissionServices {
                swarm_mode: Some(swarm_mode),
                ..Default::default()
            },
        );

        let cancel = CancelToken::new();
        let display = ToolInputDisplay::Generic {
            summary: "test".into(),
        };
        let args = serde_json::Value::Null;
        let ctx = PermissionPolicyContext {
            tool_name: "Agent",
            tool_call_id: "call-1",
            args: &args,
            approval_rule: "",
            display: &display,
            accesses: &[],
            mode: PermissionMode::Manual,
            rules: &[],
            session_approvals: &HashSet::new(),
            cancel: &cancel,
            net_class: None,
        };
        assert!(matches!(
            manager.evaluate(&ctx),
            PermissionPolicyResult::Deny { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // WorkspaceBoundaryPolicy (plan 2026-10-10-sandbox-path-permission, D2)
    // -----------------------------------------------------------------------

    /// Evaluate a boundary-policy context with the given tool, args, and rules.
    fn evaluate_boundary(
        policy: &dyn PermissionPolicy,
        tool_name: &str,
        args: &serde_json::Value,
        rules: &[PermissionRule],
    ) -> PermissionPolicyResult {
        let cancel = CancelToken::new();
        let display = ToolInputDisplay::Generic {
            summary: "test".into(),
        };
        let ctx = PermissionPolicyContext {
            tool_name,
            tool_call_id: "call-1",
            args,
            approval_rule: "",
            display: &display,
            accesses: &[],
            mode: PermissionMode::Manual,
            rules,
            session_approvals: &HashSet::new(),
            cancel: &cancel,
            net_class: None,
        };
        policy.evaluate(&ctx)
    }

    /// A `path:`-prefixed rule whose glob suffix is `suffix`.
    fn path_rule(decision: RuleDecision, suffix: &str) -> PermissionRule {
        PermissionRule {
            decision,
            scope: RuleScope::User,
            pattern: format!("path:{suffix}"),
            reason: None,
        }
    }

    /// A sandbox rooted at `root`, an unrelated outside tempdir, and a
    /// canonical outside file — the fixture every outside-shape test needs.
    fn outside_file_fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("secret.txt");
        std::fs::write(&file, "s3cret").unwrap();
        (root, outside, file.canonicalize().unwrap())
    }

    #[test]
    fn should_pass_when_workspace_paths_are_not_wired() {
        let policy = WorkspaceBoundaryPolicy::new(None);
        let args = serde_json::json!({ "path": "/etc/hosts" });
        let result = evaluate_boundary(&policy, "Read", &args, &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_tool_is_not_a_file_tool() {
        let (root, _outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let args = serde_json::json!({ "path": file });
        let result = evaluate_boundary(&policy, "Bash", &args, &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_args_carry_no_path() {
        let (root, _outside, _file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let result = evaluate_boundary(&policy, "Read", &serde_json::json!({}), &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_path_arg_is_not_a_string() {
        let (root, _outside, _file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let result = evaluate_boundary(&policy, "Read", &serde_json::json!({ "path": 42 }), &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_path_arg_is_empty() {
        let (root, _outside, _file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let result = evaluate_boundary(&policy, "Read", &serde_json::json!({ "path": "" }), &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_path_is_inside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("notes.txt");
        std::fs::write(&inside, "in").unwrap();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let args = serde_json::json!({ "path": inside });
        let result = evaluate_boundary(&policy, "Read", &args, &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_pass_when_path_is_a_relative_dotdot_escape() {
        let root = tempfile::tempdir().unwrap();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let args = serde_json::json!({ "path": "../secret.txt" });
        let result = evaluate_boundary(&policy, "Read", &args, &[]);
        assert!(matches!(result, PermissionPolicyResult::Pass));
    }

    #[test]
    fn should_ask_with_parent_grant_dir_when_outside_file_path_and_no_rule_matches() {
        let (root, outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let args = serde_json::json!({ "path": file });
        match evaluate_boundary(&policy, "Read", &args, &[]) {
            PermissionPolicyResult::Ask {
                reason, grant_dir, ..
            } => {
                assert_eq!(
                    reason.as_deref(),
                    Some("the path is outside the workspace sandbox")
                );
                assert_eq!(
                    grant_dir.as_deref(),
                    Some(outside.path().canonicalize().unwrap().as_path())
                );
            }
            other => panic!("an outside read must ask, got {other:?}"),
        }
    }

    /// Given an outside path and no rule, when each of the six boundary tools
    /// requests it, then the ask carries the grant_dir of the right category:
    /// the canonical path itself for search roots (Glob/Grep, whose argument
    /// is a directory), its parent for file targets.
    #[test]
    fn should_ask_with_grant_dir_by_tool_category_when_path_is_outside_and_no_rule_matches() {
        let (root, outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let canonical_dir = outside.path().canonicalize().unwrap();

        for tool in ["Glob", "Grep"] {
            let args = serde_json::json!({ "path": canonical_dir });
            match evaluate_boundary(&policy, tool, &args, &[]) {
                PermissionPolicyResult::Ask { grant_dir, .. } => assert_eq!(
                    grant_dir.as_deref(),
                    Some(canonical_dir.as_path()),
                    "grant_dir for {tool}"
                ),
                other => panic!("an outside {tool} must ask, got {other:?}"),
            }
        }
        for tool in ["Read", "Write", "Edit", "ReadMediaFile"] {
            let args = serde_json::json!({ "path": file });
            match evaluate_boundary(&policy, tool, &args, &[]) {
                PermissionPolicyResult::Ask { grant_dir, .. } => assert_eq!(
                    grant_dir.as_deref(),
                    Some(canonical_dir.as_path()),
                    "grant_dir for {tool}"
                ),
                other => panic!("an outside {tool} must ask, got {other:?}"),
            }
        }
    }

    #[test]
    fn should_approve_when_a_path_rule_allows_the_canonical_path() {
        let (root, _outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let glob = format!("{}/**", file.parent().unwrap().to_string_lossy());
        let rules = vec![path_rule(RuleDecision::Allow, &glob)];
        let args = serde_json::json!({ "path": file });
        match evaluate_boundary(&policy, "Read", &args, &rules) {
            PermissionPolicyResult::Approve { reason } => {
                assert_eq!(reason.as_deref(), Some("outside-sandbox path rule"));
            }
            other => panic!("a path: allow rule must approve, got {other:?}"),
        }
    }

    #[test]
    fn should_deny_when_a_path_rule_denies_the_canonical_path() {
        let (root, outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        // The user's rule is written as `path:/…/outside/**` — the `**` must
        // reach the nested file through the canonical path (the policy matches
        // the canonical form, so the glob is built from the canonical dir).
        let glob = format!(
            "{}/**",
            outside.path().canonicalize().unwrap().to_string_lossy()
        );
        let rules = vec![path_rule(RuleDecision::Deny, &glob)];
        let args = serde_json::json!({ "path": file });
        match evaluate_boundary(&policy, "Read", &args, &rules) {
            PermissionPolicyResult::Deny { reason, message } => {
                assert_eq!(reason.as_deref(), Some("outside-sandbox path rule"));
                let message = message.expect("a deny carries a user-visible message");
                assert!(
                    message.contains(
                        &outside
                            .path()
                            .canonicalize()
                            .unwrap()
                            .to_string_lossy()
                            .to_string()
                    ),
                    "the message must name the matching pattern: {message}"
                );
                assert!(message.contains("is denied by rule"), "message: {message}");
            }
            other => panic!("a path: deny rule must deny, got {other:?}"),
        }
    }

    #[test]
    fn should_ask_when_a_path_rule_asks_for_the_canonical_path() {
        let (root, outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let glob = format!(
            "{}/**",
            outside.path().canonicalize().unwrap().to_string_lossy()
        );
        let rules = vec![path_rule(RuleDecision::Ask, &glob)];
        let args = serde_json::json!({ "path": file });
        match evaluate_boundary(&policy, "Read", &args, &rules) {
            PermissionPolicyResult::Ask {
                reason, grant_dir, ..
            } => {
                assert_eq!(reason.as_deref(), Some("outside-sandbox path rule"));
                assert!(grant_dir.is_some(), "an ask rule still grants a dir");
            }
            other => panic!("a path: ask rule must ask, got {other:?}"),
        }
    }

    #[test]
    fn should_use_the_first_matching_path_rule_when_multiple_rules_match() {
        let (root, outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let glob = format!(
            "{}/**",
            outside.path().canonicalize().unwrap().to_string_lossy()
        );
        let args = serde_json::json!({ "path": file });

        // allow first, deny second → the first (allow) wins.
        let rules = vec![
            path_rule(RuleDecision::Allow, &glob),
            path_rule(RuleDecision::Deny, &glob),
        ];
        assert!(
            matches!(
                evaluate_boundary(&policy, "Read", &args, &rules),
                PermissionPolicyResult::Approve { .. }
            ),
            "the first matching rule must win"
        );
        // deny first, allow second → the first (deny) wins.
        let rules = vec![
            path_rule(RuleDecision::Deny, &glob),
            path_rule(RuleDecision::Allow, &glob),
        ];
        assert!(
            matches!(
                evaluate_boundary(&policy, "Read", &args, &rules),
                PermissionPolicyResult::Deny { .. }
            ),
            "the first matching rule must win"
        );
    }

    #[test]
    fn should_ask_with_default_reason_when_no_path_rule_matches() {
        let (root, _outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        // A path: rule scoped to an unrelated directory does not match → the
        // default outside ask applies, the allow rule is skipped.
        let rules = vec![path_rule(RuleDecision::Allow, "/elsewhere/**")];
        let args = serde_json::json!({ "path": file });
        match evaluate_boundary(&policy, "Read", &args, &rules) {
            PermissionPolicyResult::Ask { reason, .. } => assert_eq!(
                reason.as_deref(),
                Some("the path is outside the workspace sandbox")
            ),
            other => panic!("a non-matching path: rule must not decide, got {other:?}"),
        }
    }

    #[test]
    fn should_ignore_rules_without_the_path_prefix_when_evaluating() {
        let (root, _outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        // A plain tool-name deny rule must not be interpreted as a path rule
        // — this policy only speaks the `path:` prefix (plan D2).
        let rules = vec![PermissionRule {
            decision: RuleDecision::Deny,
            scope: RuleScope::User,
            pattern: "Read".to_string(),
            reason: None,
        }];
        let args = serde_json::json!({ "path": file });
        let result = evaluate_boundary(&policy, "Read", &args, &rules);
        assert!(
            matches!(result, PermissionPolicyResult::Ask { .. }),
            "tool-name rules are not path rules: {result:?}"
        );
    }

    /// Given a `path:` deny rule matching an outside path, when the
    /// WorkspaceBoundaryPolicy denies it, then the deny message carries the
    /// NEVER-bypass contract (plan D4): sandbox/permission refusals must
    /// never point the model at an alternative access route.
    #[test]
    fn should_append_the_never_bypass_contract_when_a_path_rule_denies_an_outside_path() {
        let (root, outside, file) = outside_file_fixture();
        let policy =
            WorkspaceBoundaryPolicy::new(Some(Arc::new(WorkspacePaths::new(root.path().into()))));
        let glob = format!(
            "{}/**",
            outside.path().canonicalize().unwrap().to_string_lossy()
        );
        let rules = vec![path_rule(RuleDecision::Deny, &glob)];
        let args = serde_json::json!({ "path": file });
        match evaluate_boundary(&policy, "Read", &args, &rules) {
            PermissionPolicyResult::Deny { message, .. } => {
                let message = message.expect("a deny carries a user-visible message");
                assert!(
                    message.contains(OUTSIDE_SANDBOX_DENIAL_CONTRACT),
                    "the deny message must carry the NEVER-bypass contract: {message}"
                );
            }
            other => panic!("a path: deny rule must deny, got {other:?}"),
        }
    }

    /// Given the NEVER-bypass contract, when it is checked as model-facing
    /// copy, then it names no concrete tool — "other tools" is generic, and
    /// naming a specific tool would be the alternative-route hint the
    /// contract exists to forbid.
    #[test]
    fn should_not_name_concrete_tools_in_the_outside_sandbox_denial_contract() {
        for banned in [
            "Bash",
            "Grep",
            "Glob",
            "Read",
            "Write",
            "Edit",
            "ReadMediaFile",
        ] {
            assert!(
                !OUTSIDE_SANDBOX_DENIAL_CONTRACT.contains(banned),
                "the NEVER-bypass contract must not name '{banned}': \
                 {OUTSIDE_SANDBOX_DENIAL_CONTRACT}"
            );
        }
    }
}
