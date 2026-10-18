use serde::{Deserialize, Serialize};

/// Tags every message with its source. Drives compaction keep/drop, undo
/// traversal, replay rendering, and dynamic-tool stripping.
///
/// Maps to the TypeScript `PromptOrigin` discriminated union (14 variants).
/// Serialized as `{"kind": "<variant>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PromptOrigin {
    /// Real user input (typed in the prompt bar).
    #[serde(rename = "user")]
    User,
    /// User-invoked skill activation (slash command). The identity payload
    /// mirrors the TS `SkillActivationOrigin` (agent-core-v2
    /// `agent/contextMemory/types`): `trigger` distinguishes the user's
    /// slash command (`user-slash`) from model-tool activation, and the
    /// skill identity feeds the web transcript's activation chip and
    /// compaction's user-slash keep rule. All fields optional with serde
    /// defaults so pre-payload wire records (`{"kind":"skill_activation"}`)
    /// keep parsing.
    #[serde(rename = "skill_activation", rename_all = "camelCase")]
    SkillActivation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_args: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_source: Option<String>,
    },
    /// User-invoked plugin command.
    #[serde(rename = "plugin_command")]
    PluginCommand,
    /// System-reminder injected by the injection manager.
    #[serde(rename = "injection")]
    Injection,
    /// Shell command input/output.
    #[serde(rename = "shell_command")]
    ShellCommand,
    /// Compaction summary message (the handoff note).
    #[serde(rename = "compaction_summary")]
    CompactionSummary,
    /// System trigger (e.g. context overflow recovery).
    #[serde(rename = "system_trigger")]
    SystemTrigger,
    /// Background task completion notification. Carries the identity the
    /// dedup key derives from (TS `TaskOrigin`): the task id, the terminal
    /// status word, and the notification id `task:{task_id}:{status}`.
    /// No serde defaults — a half-empty origin is dirty data that the
    /// upstream warn-and-skip logic (restore replay) tolerates by failing
    /// the parse, never by loading silently.
    #[serde(rename = "background_task", rename_all = "camelCase")]
    BackgroundTask {
        task_id: String,
        status: String,
        notification_id: String,
    },
    /// Cron job fired.
    #[serde(rename = "cron_job")]
    CronJob,
    /// Cron job missed (coalesced delivery).
    #[serde(rename = "cron_missed")]
    CronMissed,
    /// Hook result injected into context.
    #[serde(rename = "hook_result")]
    HookResult,
    /// Additional context from a hook.
    #[serde(rename = "hook_additional_context")]
    HookAdditionalContext,
    /// Retry of a previous prompt.
    #[serde(rename = "retry")]
    Retry,
    /// Model-switch marker (mid-turn `agent.setModel` / `session.updateMeta`
    /// with a changed model): a UI-only divider recording where the effective
    /// model changed. Never projected to the LLM (the projector drops it) and
    /// never real user input (compaction drops it); the identity fields ride
    /// the wire alongside the kind tag, mirroring the `background_task` shape.
    #[serde(rename = "model_switched", rename_all = "camelCase")]
    ModelSwitched { from: String, to: String },
}

impl PromptOrigin {
    /// Whether this origin represents real user input (kept during compaction).
    ///
    /// Skill activations and plugin commands are treated as first-class user
    /// intent: they must survive compaction and are reachable from undo.
    pub fn is_real_user_input(&self) -> bool {
        matches!(
            self,
            Self::User | Self::SkillActivation { .. } | Self::PluginCommand
        )
    }

    /// 返回所有变体的切片, 方便测试遍历。
    #[cfg(test)]
    fn all_variants() -> Vec<PromptOrigin> {
        vec![
            PromptOrigin::User,
            tests::skill_activation_origin(),
            PromptOrigin::PluginCommand,
            PromptOrigin::Injection,
            PromptOrigin::ShellCommand,
            PromptOrigin::CompactionSummary,
            PromptOrigin::SystemTrigger,
            PromptOrigin::BackgroundTask {
                task_id: "t".into(),
                status: "completed".into(),
                notification_id: "task:t:completed".into(),
            },
            PromptOrigin::CronJob,
            PromptOrigin::CronMissed,
            PromptOrigin::HookResult,
            PromptOrigin::HookAdditionalContext,
            PromptOrigin::Retry,
            PromptOrigin::ModelSwitched {
                from: "kimi-k2".into(),
                to: "claude-sonnet-4".into(),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::PromptOrigin;
    use serde_json::json;

    // 期望的 serde kind 标签, 与 `#[serde(rename = ...)]` 一致。
    fn expected_kind(origin: &PromptOrigin) -> &'static str {
        match origin {
            PromptOrigin::User => "user",
            PromptOrigin::SkillActivation { .. } => "skill_activation",
            PromptOrigin::PluginCommand => "plugin_command",
            PromptOrigin::Injection => "injection",
            PromptOrigin::ShellCommand => "shell_command",
            PromptOrigin::CompactionSummary => "compaction_summary",
            PromptOrigin::SystemTrigger => "system_trigger",
            PromptOrigin::BackgroundTask { .. } => "background_task",
            PromptOrigin::CronJob => "cron_job",
            PromptOrigin::CronMissed => "cron_missed",
            PromptOrigin::HookResult => "hook_result",
            PromptOrigin::HookAdditionalContext => "hook_additional_context",
            PromptOrigin::Retry => "retry",
            PromptOrigin::ModelSwitched { .. } => "model_switched",
        }
    }

    #[test]
    fn should_return_true_when_origin_is_real_user_input() {
        // 只有 User / SkillActivation / PluginCommand 属于真实用户输入。
        assert!(PromptOrigin::User.is_real_user_input());
        assert!(skill_activation_origin().is_real_user_input());
        assert!(PromptOrigin::PluginCommand.is_real_user_input());
    }

    #[test]
    fn should_return_false_when_origin_is_not_real_user_input() {
        // 其余 10 个变体都不算真实用户输入, 压缩时可被丢弃。
        for origin in &PromptOrigin::all_variants() {
            if matches!(
                origin,
                PromptOrigin::User
                    | PromptOrigin::SkillActivation { .. }
                    | PromptOrigin::PluginCommand
            ) {
                continue;
            }
            assert!(
                !origin.is_real_user_input(),
                "{origin:?} 不应被识别为真实用户输入"
            );
        }
    }

    #[test]
    fn should_serialize_with_renamed_kind_tag_when_converted_to_json() {
        // 每个变体序列化后都应是 {"kind": "<snake_case>"}。
        for origin in &PromptOrigin::all_variants() {
            let value = serde_json::to_value(origin).unwrap();
            let expected = match origin {
                // The notification variant carries its identity payload
                // alongside the kind tag (camelCase, TS TaskOrigin shape).
                PromptOrigin::BackgroundTask {
                    task_id,
                    status,
                    notification_id,
                } => json!({
                    "kind": "background_task",
                    "taskId": task_id,
                    "status": status,
                    "notificationId": notification_id,
                }),
                // The model-switch marker carries its identity payload
                // alongside the kind tag (camelCase, TS wire shape).
                PromptOrigin::ModelSwitched { from, to } => json!({
                    "kind": "model_switched",
                    "from": from,
                    "to": to,
                }),
                // The skill-activation origin carries its identity payload
                // alongside the kind tag (camelCase, TS SkillActivationOrigin shape).
                PromptOrigin::SkillActivation {
                    activation_id,
                    skill_name,
                    trigger,
                    skill_args,
                    skill_path,
                    skill_source,
                } => json!({
                    "kind": "skill_activation",
                    "activationId": activation_id,
                    "skillName": skill_name,
                    "trigger": trigger,
                    "skillArgs": skill_args,
                    "skillPath": skill_path,
                    "skillSource": skill_source,
                }),
                _ => json!({ "kind": expected_kind(origin) }),
            };
            assert_eq!(value, expected, "{origin:?} 序列化结果不符");
        }
    }

    #[test]
    fn should_deserialize_from_kind_tag_when_parsed_from_json() {
        // 从 {"kind": "..."} 反序列化应得到对应变体。
        for origin in &PromptOrigin::all_variants() {
            let json_str = serde_json::to_string(origin).unwrap();
            let back: PromptOrigin = serde_json::from_str(&json_str).unwrap();
            assert_eq!(&back, origin, "{origin:?} 反序列化结果不符");
        }
    }

    #[test]
    fn should_roundtrip_through_json_when_serialized_and_deserialized() {
        // 所有变体经 序列化->反序列化 后应保持不变。
        for origin in &PromptOrigin::all_variants() {
            let bytes = serde_json::to_vec(origin).unwrap();
            let back: PromptOrigin = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(&back, origin, "{origin:?} JSON 往返不一致");
        }
    }

    #[test]
    fn should_compare_equal_when_variants_match() {
        // 相同变体应相等。
        assert_eq!(PromptOrigin::User, PromptOrigin::User);
        assert_eq!(
            PromptOrigin::CompactionSummary,
            PromptOrigin::CompactionSummary
        );
        assert_eq!(PromptOrigin::CronMissed, PromptOrigin::CronMissed);
    }

    #[test]
    fn should_differ_when_variants_differ() {
        // 不同变体应不相等。
        assert_ne!(PromptOrigin::User, skill_activation_origin());
        assert_ne!(PromptOrigin::Injection, PromptOrigin::ShellCommand);
        assert_ne!(PromptOrigin::CronJob, PromptOrigin::CronMissed);
        assert_ne!(
            PromptOrigin::HookResult,
            PromptOrigin::HookAdditionalContext
        );
    }

    #[test]
    fn should_produce_equal_value_when_cloned() {
        // Clone 后应与原值相等。
        for origin in &PromptOrigin::all_variants() {
            let cloned = origin.clone();
            assert_eq!(&cloned, origin, "{origin:?} clone 后不相等");
        }
    }

    #[test]
    fn should_format_with_debug_when_inspected() {
        // Debug 派生应能生成非空字符串, 且包含变体名。
        let debug_str = format!("{:?}", PromptOrigin::User);
        assert!(
            debug_str.contains("User"),
            "Debug 输出应包含变体名: {debug_str}"
        );

        let debug_str = format!("{:?}", notification_origin("t"));
        assert!(
            debug_str.contains("BackgroundTask"),
            "Debug 输出应包含变体名: {debug_str}"
        );
    }

    /// A minimal background-task notification origin for the shared-variant
    /// tests (the identity fields are payload-only here).
    fn notification_origin(task_id: &str) -> PromptOrigin {
        PromptOrigin::BackgroundTask {
            task_id: task_id.to_string(),
            status: "completed".to_string(),
            notification_id: format!("task:{task_id}:completed"),
        }
    }

    #[test]
    fn should_serialize_the_background_task_origin_with_camel_case_identity_fields() {
        // Given a notification origin carrying the dedup identity
        let origin = PromptOrigin::BackgroundTask {
            task_id: "process-abc12345".into(),
            status: "completed".into(),
            notification_id: "task:process-abc12345:completed".into(),
        };
        // When serialized, the shape is the TS TaskOrigin wire form
        let value = serde_json::to_value(&origin).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "background_task",
                "taskId": "process-abc12345",
                "status": "completed",
                "notificationId": "task:process-abc12345:completed",
            })
        );
    }

    #[test]
    fn should_parse_the_ts_wire_shape_when_deserializing_a_notification_origin() {
        // Given the TS wire JSON for a notification origin
        let raw = serde_json::json!({
            "kind": "background_task",
            "taskId": "question-9f1e2d3c",
            "status": "completed",
            "notificationId": "task:question-9f1e2d3c:completed",
        });
        // When parsed, the identity fields survive verbatim
        let parsed: PromptOrigin = serde_json::from_value(raw).unwrap();
        assert!(matches!(
            parsed,
            PromptOrigin::BackgroundTask { ref task_id, ref status, .. }
                if task_id == "question-9f1e2d3c" && status == "completed"
        ));
    }

    #[test]
    fn should_not_be_real_user_input_when_origin_is_a_notification() {
        // A notification is engine-synthesized, never real user input.
        assert!(!notification_origin("t").is_real_user_input());
    }

    #[test]
    fn should_reject_a_half_empty_origin_when_deserializing() {
        // A background_task record missing its identity is dirty data — the
        // parse fails and upstream warn-and-skip logic tolerates it (no
        // serde defaults: a half-empty origin must never load silently).
        let raw = serde_json::json!({ "kind": "background_task" });
        assert!(serde_json::from_value::<PromptOrigin>(raw).is_err());
    }

    /// A minimal model-switch marker origin for the shared-variant tests.
    fn model_switch_origin() -> PromptOrigin {
        PromptOrigin::ModelSwitched {
            from: "kimi-k2".into(),
            to: "claude-sonnet-4".into(),
        }
    }

    #[test]
    fn should_serialize_the_model_switched_origin_with_camel_case_fields() {
        // Given a model-switch marker origin
        let origin = model_switch_origin();
        // When serialized, the shape is the TS wire form
        let value = serde_json::to_value(&origin).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "model_switched",
                "from": "kimi-k2",
                "to": "claude-sonnet-4",
            })
        );
    }

    #[test]
    fn should_parse_the_ts_wire_shape_when_deserializing_a_model_switch_origin() {
        // Given the TS wire JSON for a model-switch marker
        let raw = serde_json::json!({
            "kind": "model_switched",
            "from": "kimi-k2",
            "to": "claude-sonnet-4",
        });
        // When parsed, the identity fields survive verbatim
        let parsed: PromptOrigin = serde_json::from_value(raw).unwrap();
        assert!(matches!(
            parsed,
            PromptOrigin::ModelSwitched { ref from, ref to }
                if from == "kimi-k2" && to == "claude-sonnet-4"
        ));
    }

    #[test]
    fn should_not_be_real_user_input_when_origin_is_a_model_switch() {
        // A model-switch marker is UI-only, never real user input.
        assert!(!model_switch_origin().is_real_user_input());
    }

    /// The payload identity fields of a user-slash skill activation (the
    /// shape the web transcript and compaction rules consume). `pub(super)`
    /// so the parent-scope `all_variants` test helper can reuse it.
    pub(super) fn skill_activation_origin() -> PromptOrigin {
        PromptOrigin::SkillActivation {
            activation_id: Some("act-018f6b2f".into()),
            skill_name: Some("commit".into()),
            trigger: Some("user-slash".into()),
            skill_args: Some("fix the flaky test".into()),
            skill_path: Some("/tmp/skills/commit".into()),
            skill_source: Some("project".into()),
        }
    }

    #[test]
    fn should_serialize_a_skill_activation_origin_with_camel_case_payload() {
        // Given a skill-activation origin carrying its identity payload
        // When serialized, the shape is the TS SkillActivationOrigin wire form
        assert_eq!(
            serde_json::to_value(skill_activation_origin()).unwrap(),
            json!({
                "kind": "skill_activation",
                "activationId": "act-018f6b2f",
                "skillName": "commit",
                "trigger": "user-slash",
                "skillArgs": "fix the flaky test",
                "skillPath": "/tmp/skills/commit",
                "skillSource": "project",
            })
        );
    }

    #[test]
    fn should_parse_a_legacy_payload_free_skill_activation_origin() {
        // Given a pre-payload wire record (the old unit-variant spelling)
        // When parsed, the origin loads with every identity field unset
        let parsed: PromptOrigin =
            serde_json::from_value(json!({ "kind": "skill_activation" })).unwrap();
        assert!(matches!(
            parsed,
            PromptOrigin::SkillActivation {
                skill_name: None,
                trigger: None,
                ..
            }
        ));
    }

    #[test]
    fn should_serialize_a_skill_activation_origin_without_payload_fields() {
        // Given a payload-free skill-activation origin
        let origin = PromptOrigin::SkillActivation {
            activation_id: None,
            skill_name: None,
            trigger: None,
            skill_args: None,
            skill_path: None,
            skill_source: None,
        };
        // When serialized, only the kind tag appears (the legacy wire shape)
        assert_eq!(
            serde_json::to_value(&origin).unwrap(),
            json!({ "kind": "skill_activation" })
        );
    }
}
