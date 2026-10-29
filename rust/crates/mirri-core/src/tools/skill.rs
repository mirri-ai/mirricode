//! `SkillTool` — invoke a registered skill from the current skill listing.
//!
//! Mirrors the TypeScript `SkillTool`: the model calls this to load an
//! inline skill's `SKILL.md` content as instructions. The tool resolves the
//! skill by name from the [`SkillCatalog`], reads its content, and returns
//! it for the model to follow. Unknown skills produce an error so the model
//! can correct its request.
//!
//! The skill content is returned both as the tool result output (so the model
//! sees it in the tool response) and as a system message injection via
//! [`ToolResult::injection`] — the turn flow appends the injected content as
//! a `<system-reminder>` after the tool result, mirroring the TS "steer"
//! delivery mechanism.

use std::sync::Arc;

use crate::error::EngineResult;
use crate::session::events::{EventBus, SessionEvent};
use crate::skill::SkillCatalog;
use crate::tool::{
    ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay, ToolResult,
};

/// `Skill` — invoke a registered skill from the current skill listing.
///
/// The tool holds `Arc<SkillCatalog>` — the workspace-scoped catalog that
/// indexes discovered `SKILL.md` files. The catalog uses interior mutability
/// (`RwLock`) for discovery, so reads behind an `Arc` are safe.
pub struct SkillTool {
    /// The workspace-scoped skill catalog backing this tool.
    catalog: Arc<SkillCatalog>,
    /// The session's event bus. When attached, a successful activation
    /// announces the skill on the wire (`event.skill.activated`, trigger
    /// `model-tool`).
    event_bus: Option<Arc<EventBus>>,
}

impl SkillTool {
    /// Create the tool bound to the workspace-scoped skill catalog.
    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        Self {
            catalog,
            event_bus: None,
        }
    }

    /// Attach the session's event bus so skill activations are announced.
    pub fn with_event_bus(mut self, event_bus: Option<Arc<EventBus>>) -> Self {
        self.event_bus = event_bus;
        self
    }
}

/// The TS `SkillSource` wire vocabulary for a catalog [`SkillSource`].
fn skill_source_wire(source: crate::skill::SkillSource) -> &'static str {
    match source {
        crate::skill::SkillSource::Builtin => "builtin",
        crate::skill::SkillSource::User => "user",
        crate::skill::SkillSource::Project => "project",
        crate::skill::SkillSource::Plugin => "extra",
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        "Invoke a registered skill from the current skill listing. BLOCKING \
         REQUIREMENT: when a skill from the listing matches the user's \
         request, you MUST call this tool (not free-form text). Do not \
         re-invoke a skill to repeat work already done: if a \
         <kimi-skill-loaded> block for it with the same args is already \
         present in the conversation, follow those instructions directly \
         instead of calling the tool again. Do call the tool again when you \
         need the skill with different arguments — the loaded block was \
         expanded with the earlier args and will not reflect new inputs."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "The exact name of the skill to invoke, spelled as it appears in the current skill listing (e.g. \"commit\", \"pdf\")."
                    },
                    "args": {
                        "type": "string",
                        "description": "Optional argument string for the skill, written like a command line (e.g. `-m \"fix bug\"`, `123`, a file path). It is split on whitespace (quotes group a token) and expanded into the skill's placeholders ($NAME, $1, $ARGUMENTS); if the skill body has no placeholders, the whole string is still appended as a trailing ARGUMENTS: line. Omit it only when there is nothing to pass."
                    }
                },
                "required": ["skill"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(skill_name) = input.get("skill").and_then(serde_json::Value::as_str) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "Skill requires a string 'skill' argument".to_string(),
            }));
        };

        let skill_args = input
            .get("args")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        let skill_name = skill_name.to_string();
        let catalog = Arc::clone(&self.catalog);
        let event_bus = self.event_bus.clone();
        let display_skill = skill_name.clone();
        let execution_plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::SkillCall {
                skill_name: display_skill,
                args: if skill_args.is_empty() {
                    None
                } else {
                    Some(skill_args.clone())
                },
            },
            approval_rule: "Skill".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan: execution_plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    // Resolve the skill definition from the catalog — a bare
                    // name or a plugin-qualified `plugin_id:skill_name`
                    // reference (SDR-002 / ADR-001). The not-found error
                    // echoes the ORIGINAL input reference verbatim.
                    let Some(skill_def) = catalog.resolve(&skill_name) else {
                        return ToolResult::error(format!(
                            "Skill \"{skill_name}\" not found in the current skill listing."
                        ));
                    };

                    // 用户级禁用（B8 D-32）：技能仍在 catalog 中，但不可被模型调用。
                    // The gate is evaluated AFTER resolution, so a
                    // plugin-qualified reference to a user-disabled skill is
                    // a disabled error, not a not-found — but the error
                    // message echoes the ORIGINAL input reference verbatim.
                    if catalog.is_disabled(&skill_def.name) {
                        return ToolResult::error(format!(
                            "Skill \"{skill_name}\" is disabled by the user."
                        ));
                    }

                    // 禁模型调用门（design 13 §3.4）：声明
                    // `disable_model_invocation` 的技能只能由用户触发（斜杠
                    // 激活），模型侧经 Skill 工具的调用在这里被硬拦截——
                    // 先于内容注入，错误文案取 TS skillTool.ts
                    // disableModelInvocation 分支逐字（「只能由用户触发」
                    // 方向），并回显原始输入引用（与解析门/禁用门一致）。
                    // 用户斜杠激活路径（dispatch_activate_skill）不加此门：
                    // 该标志只约束模型侧（US1.2）。frontmatter 声明该键的
                    // 用户技能同样在此被拦（ADR-B6）。
                    if skill_def.disable_model_invocation {
                        return ToolResult::error(format!(
                            "Skill \"{skill_name}\" can only be triggered by the user \
                             (model invocation is disabled)."
                        ));
                    }

                    // Render the skill prompt (item J): the raw SKILL.md
                    // body, prefixed with the `<kimi-plugin-instructions>`
                    // block when the skill's plugin carries instructions —
                    // the TS `renderSkillPrompt` shape. Rendered by the
                    // formal name the resolution produced.
                    let Some(content) = catalog.render_skill_prompt(&skill_def.name) else {
                        return ToolResult::error(format!(
                            "Skill \"{skill_name}\" was found but its content could not be read."
                        ));
                    };

                    // Announce the activation on the session bus (the
                    // `skill.activated` wire event). The tool path is a
                    // model-initiated invocation, so the trigger is
                    // `model-tool`; the optional `args` stays absent when
                    // the model invoked the skill bare.
                    if let Some(event_bus) = event_bus.as_ref() {
                        event_bus.publish(SessionEvent::SkillActivated {
                            activation_id: format!("act-{}", uuid::Uuid::new_v4()),
                            skill_name: skill_def.name.clone(),
                            args: if skill_args.is_empty() {
                                None
                            } else {
                                Some(skill_args.clone())
                            },
                            trigger: "model-tool".to_string(),
                            skill_path: Some(skill_def.path.display().to_string()),
                            skill_source: Some(skill_source_wire(skill_def.source).to_string()),
                        });
                    }

                    // Build the model-visible output. The skill content is
                    // returned directly so the model can follow the
                    // instructions. If args were provided, they are appended
                    // as an ARGUMENTS: line for skills with no placeholders.
                    let output = if skill_args.is_empty() {
                        format!(
                            "Skill \"{name}\" loaded inline. Follow its instructions.\n\n{content}",
                            name = skill_def.name
                        )
                    } else {
                        format!(
                            "Skill \"{name}\" loaded inline. Follow its instructions.\n\n\
                             ARGUMENTS: {args}\n\n{content}",
                            name = skill_def.name,
                            args = skill_args,
                        )
                    };

                    // 将 skill 指令内容作为系统消息注入, 使 turn flow 能以
                    // <system-reminder> 的形式追加到上下文中。
                    let injection = if skill_args.is_empty() {
                        format!("<kimi-skill-loaded>\n{content}\n</kimi-skill-loaded>",)
                    } else {
                        format!(
                            "<kimi-skill-loaded args=\"{args}\">\n{content}\n</kimi-skill-loaded>",
                            args = skill_args,
                        )
                    };

                    let mut result = ToolResult::success(output);
                    result.injection = Some(injection);
                    result
                })
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{SKILL_MD_FILE, SkillSource};
    use crate::tool::ToolOutput;
    use std::fs;
    use tempfile::TempDir;

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    async fn execute_tool(tool: &SkillTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected runnable execution");
        };
        let ctx = crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        execute(ctx).await
    }

    #[tokio::test]
    async fn should_publish_skill_activated_with_model_tool_trigger_when_an_event_bus_is_attached()
    {
        let (_temp, catalog) = make_catalog("greet", "# Greet\nSay hi.");
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = SkillTool::new(Arc::clone(&catalog)).with_event_bus(Some(Arc::clone(&bus)));
        let result = execute_tool(
            &tool,
            serde_json::json!({ "skill": "greet", "args": "-m hi" }),
        )
        .await;
        assert!(!result.is_error, "{}", output_text(&result));

        let event = rx.try_recv().expect("skill.activated should be published");
        let SessionEvent::SkillActivated {
            activation_id,
            skill_name,
            args,
            trigger,
            skill_path,
            skill_source,
        } = event
        else {
            panic!("expected SkillActivated, got {event:?}");
        };
        assert!(activation_id.starts_with("act-"));
        assert_eq!(skill_name, "greet");
        assert_eq!(args.as_deref(), Some("-m hi"));
        assert_eq!(trigger, "model-tool");
        assert!(skill_path.unwrap().contains("greet"));
        assert_eq!(skill_source.as_deref(), Some("project"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn should_leave_args_absent_when_the_skill_is_invoked_bare() {
        let (_temp, catalog) = make_catalog("plain", "# Plain\nDo it.");
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = SkillTool::new(Arc::clone(&catalog)).with_event_bus(Some(bus));
        let result = execute_tool(&tool, serde_json::json!({ "skill": "plain" })).await;
        assert!(!result.is_error);
        let SessionEvent::SkillActivated { args, .. } =
            rx.try_recv().expect("skill.activated should be published")
        else {
            unreachable!();
        };
        assert!(args.is_none());
    }

    #[tokio::test]
    async fn should_not_publish_skill_activated_when_the_skill_is_unknown() {
        let (_temp, catalog) = make_catalog("greet", "# Greet\nSay hi.");
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = SkillTool::new(Arc::clone(&catalog)).with_event_bus(Some(bus));
        let result = execute_tool(&tool, serde_json::json!({ "skill": "nonexistent" })).await;
        assert!(result.is_error);
        assert!(rx.try_recv().is_err(), "no event expected on failure");
    }

    /// Create a temp directory with a single skill and return a catalog
    /// that has discovered it.
    fn make_catalog(skill_name: &str, content: &str) -> (TempDir, Arc<SkillCatalog>) {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("skills").join(skill_name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SKILL_MD_FILE), content).unwrap();

        let catalog = Arc::new(SkillCatalog::new());
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();
        (temp, catalog)
    }

    #[tokio::test]
    async fn should_return_skill_content_when_skill_exists() {
        let (_temp, catalog) = make_catalog(
            "greet",
            "---\ndescription: Greets the user.\n---\n# Greet\n\nSay hello warmly.",
        );
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "greet" })).await;

        assert!(!result.is_error, "result: {:?}", result);
        let message = output_text(&result);
        assert!(message.contains("Skill \"greet\" loaded inline"));
        assert!(message.contains("# Greet"));
        assert!(message.contains("Say hello warmly"));
    }

    #[tokio::test]
    async fn should_append_arguments_line_when_args_provided() {
        let (_temp, catalog) = make_catalog("commit", "# Commit\n\nCreate a commit message.");
        let tool = SkillTool::new(catalog);

        let result = execute_tool(
            &tool,
            serde_json::json!({ "skill": "commit", "args": "-m \"fix bug\"" }),
        )
        .await;

        assert!(!result.is_error);
        let message = output_text(&result);
        assert!(message.contains("ARGUMENTS: -m \"fix bug\""));
        assert!(message.contains("# Commit"));
    }

    #[tokio::test]
    async fn should_return_error_when_skill_not_found() {
        let catalog = Arc::new(SkillCatalog::new());
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "nonexistent" })).await;

        assert!(result.is_error);
        let message = output_text(&result);
        assert!(message.contains("not found"));
        assert!(message.contains("nonexistent"));
    }

    #[tokio::test]
    async fn should_return_an_execution_error_when_skill_argument_is_missing() {
        let catalog = Arc::new(SkillCatalog::new());
        let tool = SkillTool::new(catalog);

        let execution = tool.resolve_execution(serde_json::json!({})).unwrap();
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution for missing skill argument");
        };
        assert!(error.message.contains("'skill'"));
    }

    #[tokio::test]
    async fn should_use_skill_call_display_when_invoking_a_skill() {
        let (_temp, catalog) = make_catalog("pdf", "# PDF\n\nHandle PDFs.");
        let tool = SkillTool::new(catalog);

        let execution = tool
            .resolve_execution(serde_json::json!({ "skill": "pdf" }))
            .unwrap();
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };

        match &plan.display {
            ToolInputDisplay::SkillCall { skill_name, .. } => {
                assert_eq!(skill_name, "pdf");
            }
            other => panic!("expected SkillCall display, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_not_include_arguments_line_when_args_omitted() {
        let (_temp, catalog) = make_catalog("plain", "# Plain\n\nNo args needed.");
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "plain" })).await;

        assert!(!result.is_error);
        let message = output_text(&result);
        assert!(!message.contains("ARGUMENTS:"));
        assert!(message.contains("# Plain"));
    }

    /// 当 skill 被调用时, 应将 skill 指令内容通过 `injection` 字段注入,
    /// 使 turn flow 能以 `<system-reminder>` 的形式追加到上下文。
    #[tokio::test]
    async fn should_inject_skill_instructions_when_skill_is_invoked() {
        let (_temp, catalog) = make_catalog(
            "commit",
            "# Commit\n\nCreate a commit message with conventional format.",
        );
        let tool = SkillTool::new(catalog);

        let result = execute_tool(
            &tool,
            serde_json::json!({ "skill": "commit", "args": "-m \"fix bug\"" }),
        )
        .await;

        assert!(!result.is_error);
        // injection 字段应包含 skill 内容。
        let injection = result.injection.as_ref().expect("injection 字段应被设置");
        assert!(
            injection.contains("<kimi-skill-loaded"),
            "injection 应包含 kimi-skill-loaded 标签: {injection}"
        );
        assert!(
            injection.contains("# Commit"),
            "injection 应包含 skill 内容: {injection}"
        );
        assert!(
            injection.contains("Create a commit message"),
            "injection 应包含 skill 正文: {injection}"
        );
        assert!(
            injection.contains("args=\"-m \"fix bug\"\""),
            "injection 应包含 args 属性: {injection}"
        );
    }

    /// Given a plugin skill with `skillInstructions`, when the tool loads it,
    /// then the instructions ride both the tool output and the injection as
    /// a `<kimi-plugin-instructions>` block — the item-J consumption point.
    #[tokio::test]
    async fn should_prepend_plugin_instructions_when_invoking_plugin_skill() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("skills").join("greeter");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SKILL_MD_FILE), "# Greeter\nDo the demo thing.").unwrap();

        let catalog = Arc::new(SkillCatalog::new());
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &crate::skill::SkillPluginContext {
                    id: "demo".into(),
                    instructions: Some("Always be helpful.".into()),
                },
            )
            .unwrap();
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "greeter" })).await;

        assert!(!result.is_error, "{}", output_text(&result));
        let message = output_text(&result);
        assert!(
            message.contains(
                "<kimi-plugin-instructions plugin=\"demo\">\nAlways be helpful.\n</kimi-plugin-instructions>"
            ),
            "the rendered prompt must carry the instructions block: {message}"
        );
        let injection = result.injection.as_ref().expect("injection set");
        assert!(
            injection.contains("<kimi-plugin-instructions plugin=\"demo\">"),
            "the injection must carry the instructions block: {injection}"
        );
    }

    /// 当 skill 被调用且未提供 args 时, injection 不应包含 args 属性。
    #[tokio::test]
    async fn should_inject_skill_instructions_without_args_when_args_omitted() {
        let (_temp, catalog) = make_catalog("greet", "# Greet\n\nSay hello.");
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "greet" })).await;

        assert!(!result.is_error);
        let injection = result.injection.as_ref().expect("injection 字段应被设置");
        assert!(
            injection.contains("<kimi-skill-loaded>"),
            "injection 应包含无 args 的 kimi-skill-loaded 标签: {injection}"
        );
        assert!(
            !injection.contains("args="),
            "无 args 时 injection 不应包含 args 属性: {injection}"
        );
    }

    /// 当 skill 不存在时, 不应设置 injection 字段。
    #[tokio::test]
    async fn should_not_inject_when_skill_not_found() {
        let catalog = Arc::new(SkillCatalog::new());
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "nonexistent" })).await;

        assert!(result.is_error);
        assert!(
            result.injection.is_none(),
            "skill 不存在时不应设置 injection"
        );
    }

    /// 当技能被用户禁用时, 工具应返回错误且不注入内容。
    #[tokio::test]
    async fn should_return_error_and_not_inject_when_skill_is_disabled() {
        let (_temp, catalog) = make_catalog("greet", "# Greet\nSay hi.");
        catalog.set_enabled("greet", false);
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "greet" })).await;

        assert!(result.is_error);
        assert!(output_text(&result).contains("disabled"));
        assert!(result.injection.is_none());
    }

    // ===== plugin-qualified skill references (SDR-002 / ADR-001) =====

    /// Create a temp directory with a single plugin skill discovered under a
    /// plugin attribution and return a catalog that has indexed it.
    fn make_plugin_catalog(
        plugin_id: &str,
        skill_name: &str,
        content: &str,
    ) -> (TempDir, Arc<SkillCatalog>) {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("skills").join(skill_name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SKILL_MD_FILE), content).unwrap();

        let catalog = Arc::new(SkillCatalog::new());
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &crate::skill::SkillPluginContext {
                    id: plugin_id.into(),
                    instructions: None,
                },
            )
            .unwrap();
        (temp, catalog)
    }

    /// Given a plugin skill, when the tool is invoked with the
    /// plugin-qualified name, then it loads and the output echoes the formal
    /// bare name.
    #[tokio::test]
    async fn should_load_plugin_skill_when_invoked_with_a_qualified_name() {
        let (_temp, catalog) =
            make_plugin_catalog("demo", "greeter", "# Greeter\nDo the demo thing.");
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "demo:greeter" })).await;

        assert!(!result.is_error, "{}", output_text(&result));
        let message = output_text(&result);
        assert!(
            message.contains("Skill \"greeter\" loaded inline"),
            "the output must echo the formal bare name: {message}"
        );
        assert!(
            !message.contains("Skill \"demo:greeter\" loaded inline"),
            "the output must not echo the qualified reference: {message}"
        );
        assert!(message.contains("# Greeter"));
    }

    /// Given a plugin skill, when the tool is invoked with the
    /// plugin-qualified name while an event bus is attached, then the
    /// activation event announces the formal bare name.
    #[tokio::test]
    async fn should_announce_formal_name_when_invoked_with_a_qualified_name() {
        let (_temp, catalog) =
            make_plugin_catalog("demo", "greeter", "# Greeter\nDo the demo thing.");
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = SkillTool::new(catalog).with_event_bus(Some(bus));

        let result = execute_tool(&tool, serde_json::json!({ "skill": "demo:greeter" })).await;
        assert!(!result.is_error, "{}", output_text(&result));

        let event = rx.try_recv().expect("skill.activated should be published");
        let SessionEvent::SkillActivated {
            skill_name,
            skill_source,
            ..
        } = event
        else {
            panic!("expected SkillActivated, got {event:?}");
        };
        assert_eq!(skill_name, "greeter");
        assert_eq!(skill_source.as_deref(), Some("extra"));
    }

    /// Given a plugin skill that is user-disabled, when the tool is invoked
    /// with the plugin-qualified name, then the disabled gate fires — not
    /// the not-found error (the gate is evaluated after resolution), and
    /// the error echoes the ORIGINAL input reference verbatim.
    #[tokio::test]
    async fn should_return_disabled_error_when_a_qualified_skill_is_user_disabled() {
        let (_temp, catalog) = make_plugin_catalog("demo", "greeter", "# Greeter\nbody");
        catalog.set_enabled("greeter", false);
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "demo:greeter" })).await;

        assert!(result.is_error);
        let message = output_text(&result);
        assert_eq!(
            message, "Skill \"demo:greeter\" is disabled by the user.",
            "the disabled error must echo the original input reference: {message}"
        );
        assert!(
            !message.contains("not found"),
            "a resolved-but-disabled skill is not a not-found: {message}"
        );
        assert!(result.injection.is_none());
    }

    /// Given a qualified reference that resolves to nothing, when the tool
    /// is invoked with it, then the not-found error echoes the original
    /// input reference verbatim.
    #[tokio::test]
    async fn should_echo_the_original_reference_when_a_qualified_name_fails_to_resolve() {
        let catalog = Arc::new(SkillCatalog::new());
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "nope:missing" })).await;

        assert!(result.is_error);
        let message = output_text(&result);
        assert_eq!(
            message,
            "Skill \"nope:missing\" not found in the current skill listing."
        );
    }

    // ===== model-invocation gate (design 13 §3.4, M2 model plane) =====

    /// Given the builtin mcp-config (disable_model_invocation = true), when
    /// the model invokes it through the Skill tool, then the execution
    /// returns the user-trigger-only error with the TS literal copy, echoes
    /// the original input name, and injects nothing (V3 — the gate half).
    #[tokio::test]
    async fn should_return_the_user_trigger_error_when_a_builtin_disables_model_invocation() {
        let catalog = Arc::new(SkillCatalog::new());
        catalog.register_builtin(crate::skill::builtin::builtin_skills());
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "mcp-config" })).await;

        assert!(result.is_error);
        assert_eq!(
            output_text(&result),
            "Skill \"mcp-config\" can only be triggered by the user (model invocation is disabled).",
            "the TS skillTool.ts disableModelInvocation branch copy, verbatim"
        );
        assert!(
            result.injection.is_none(),
            "a model-gated skill must not reach the content-injection stage"
        );
    }

    /// Given the builtin write-goal (model-invocable), when the model
    /// invokes it, then the full embedded body rides the current turn —
    /// served through the builtin content table and injected as the
    /// `<kimi-skill-loaded>` block (V3 — the SkillTool half).
    #[tokio::test]
    async fn should_inject_the_embedded_body_when_invoking_the_write_goal_builtin() {
        let catalog = Arc::new(SkillCatalog::new());
        catalog.register_builtin(crate::skill::builtin::builtin_skills());
        let tool = SkillTool::new(catalog);
        let body = crate::skill::builtin::builtin_skills()
            .into_iter()
            .find(|skill| skill.def.name == "write-goal")
            .expect("write-goal entry")
            .content;

        let result = execute_tool(&tool, serde_json::json!({ "skill": "write-goal" })).await;

        assert!(!result.is_error, "{}", output_text(&result));
        let injection = result.injection.as_ref().expect("injection set");
        assert!(
            injection.contains(&format!(
                "<kimi-skill-loaded>\n{body}\n</kimi-skill-loaded>"
            )),
            "the full embedded body rides the current turn: {injection}"
        );
    }

    /// Given a skill that is BOTH user-disabled and model-gated, when the
    /// model invokes it, then the user-disabled gate fires — the gate chain
    /// order is resolve → user-disabled → model-gated (§3.4), so the error
    /// reports the user disable, not the model gate.
    #[tokio::test]
    async fn should_report_user_disabled_before_model_gated_when_a_skill_hits_both_gates() {
        let (_temp, catalog) = make_catalog(
            "gated",
            "---\ndescription: gated\ndisable-model-invocation: true\n---\n# Gated\n",
        );
        catalog.set_enabled("gated", false);
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "gated" })).await;

        assert!(result.is_error);
        let message = output_text(&result);
        assert_eq!(
            message, "Skill \"gated\" is disabled by the user.",
            "the user-disabled gate precedes the model-invocation gate: {message}"
        );
        assert!(
            !message.contains("model invocation"),
            "the model gate must not fire for a user-disabled skill: {message}"
        );
    }

    /// Given a USER skill whose frontmatter declares
    /// `disable-model-invocation: true`, when the model invokes it, then the
    /// gate fires for user skills too — the parsed flag feeds the same
    /// SkillTool gate (ADR-B6, the TS US1.2 semantics completed).
    #[tokio::test]
    async fn should_block_model_invocation_when_a_user_skill_declares_the_gate_in_frontmatter() {
        let (_temp, catalog) = make_catalog(
            "gated",
            "---\ndescription: gated\ndisable-model-invocation: true\n---\n# Gated\n",
        );
        let tool = SkillTool::new(catalog);

        let result = execute_tool(&tool, serde_json::json!({ "skill": "gated" })).await;

        assert!(result.is_error);
        assert_eq!(
            output_text(&result),
            "Skill \"gated\" can only be triggered by the user (model invocation is disabled).",
            "the frontmatter-declared gate blocks user skills as well"
        );
        assert!(result.injection.is_none());
    }
}
