use super::*;
use crate::capability_gap::{ModelCapabilities, ProfileInfo};
use crate::context::ContextMessage;
use crate::context_memory::ContextMemory;
use crate::llm::LlmUsage;
use crate::message::{ContentPart, ImageUrlData, Message};
use crate::origin::PromptOrigin;

struct StaticInjector {
    text: Option<String>,
}

impl Injector for StaticInjector {
    fn inject(&self, _memory: &ContextMemory) -> Option<String> {
        self.text.clone()
    }
}

#[test]
fn should_not_append_reminders_when_manager_has_no_injectors() {
    let manager = InjectionManager::new();
    let mut memory = ContextMemory::new();
    manager.inject(&mut memory);
    assert!(memory.history().is_empty());
}

#[test]
fn should_append_reminder_when_injector_returns_some_text() {
    let mut manager = InjectionManager::new();
    manager.add(Box::new(StaticInjector {
        text: Some("Remember to be concise.".to_string()),
    }));
    let mut memory = ContextMemory::new();
    manager.inject(&mut memory);
    assert!(!memory.history().is_empty());
}

#[test]
fn should_not_append_reminder_when_injector_returns_none() {
    let mut manager = InjectionManager::new();
    manager.add(Box::new(StaticInjector { text: None }));
    let mut memory = ContextMemory::new();
    manager.inject(&mut memory);
    assert!(memory.history().is_empty());
}

#[test]
fn should_append_reminders_in_registration_order_when_multiple_injectors_return_some() {
    let mut manager = InjectionManager::new();
    manager.add(Box::new(StaticInjector {
        text: Some("First reminder".to_string()),
    }));
    manager.add(Box::new(StaticInjector { text: None }));
    manager.add(Box::new(StaticInjector {
        text: Some("Third reminder".to_string()),
    }));
    let mut memory = ContextMemory::new();
    manager.inject(&mut memory);
    // Two reminders should be in history (the None one is skipped).
    let count = memory
        .history()
        .iter()
        .filter(|m| m.origin == Some(crate::origin::PromptOrigin::Injection))
        .count();
    assert_eq!(count, 2);
}

// -----------------------------------------------------------------------
// CapabilityGapInjector
// -----------------------------------------------------------------------

fn image_part(url: &str) -> ContentPart {
    ContentPart::ImageUrl {
        image_url: ImageUrlData {
            url: url.into(),
            detail: None,
            id: None,
        },
        detail: None,
    }
}

/// A profile whose model is known to support image input.
fn vision_profile() -> ProfileInfo {
    ProfileInfo {
        name: "vision-agent".into(),
        default_model: "gpt-4-vision".into(),
        model_capabilities: ModelCapabilities {
            image_in: true,
            ..Default::default()
        },
    }
}

fn image_gap_injector() -> CapabilityGapInjector {
    CapabilityGapInjector::new(
        ModelCapabilities {
            image_in: false,
            video_in: true,
            audio_in: true,
            tool_use: true,
            thinking: true,
        },
        vec![vision_profile()],
        vec![],
    )
}

#[test]
fn should_inject_hint_when_last_user_message_contains_modality_current_model_lacks() {
    // Given a model without image input and a user message containing an image
    let injector = image_gap_injector();
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![image_part("https://example.com/img.png")],
        PromptOrigin::User,
    );

    // When injecting
    let hint = injector.inject(&memory);

    // Then a hint naming the vision profile is produced
    let hint = hint.expect("hint should be Some");
    assert!(hint.contains("image"));
    assert!(hint.contains("vision-agent"));
    assert!(hint.contains("dispatch a subagent"));
}

#[test]
fn should_not_inject_when_model_supports_all_modalities_in_last_user_message() {
    // Given a model with image input and a user message containing an image
    let injector = CapabilityGapInjector::new(
        ModelCapabilities {
            image_in: true,
            ..Default::default()
        },
        vec![vision_profile()],
        vec![],
    );
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![image_part("https://example.com/img.png")],
        PromptOrigin::User,
    );

    // When injecting
    let hint = injector.inject(&memory);

    // Then no hint is produced (no gap)
    assert!(hint.is_none());
}

#[test]
fn should_not_inject_when_no_catalog_model_covers_the_gap() {
    // Given an image gap, no covering profile, and a non-empty catalog
    // snapshot whose models do not support images either
    let injector = CapabilityGapInjector::new(
        ModelCapabilities {
            image_in: false,
            ..Default::default()
        },
        vec![ProfileInfo {
            name: "text-agent".into(),
            default_model: "gpt-4".into(),
            model_capabilities: ModelCapabilities::default(),
        }],
        vec![ModelCapabilities::default()],
    );
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![image_part("https://example.com/img.png")],
        PromptOrigin::User,
    );

    // When injecting
    let hint = injector.inject(&memory);

    // Then no hint is produced (feasibility verified: dispatch would be futile)
    assert!(hint.is_none());
}

#[test]
fn should_inject_when_catalog_snapshot_is_empty_despite_no_covering_profile() {
    // Given an image gap, no covering profile, and an empty catalog snapshot
    // (standalone flow — feasibility cannot be verified)
    let injector = CapabilityGapInjector::new(
        ModelCapabilities {
            image_in: false,
            ..Default::default()
        },
        vec![ProfileInfo {
            name: "text-agent".into(),
            default_model: "gpt-4".into(),
            model_capabilities: ModelCapabilities::default(),
        }],
        vec![],
    );
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![image_part("https://example.com/img.png")],
        PromptOrigin::User,
    );

    // When injecting
    let hint = injector.inject(&memory);

    // Then a hint is still produced (empty catalog is permissive, TS parity)
    assert!(hint.is_some());
}

#[test]
fn should_not_inject_when_history_has_no_user_message() {
    // Given a model with an image gap but a history with no user message
    let injector = image_gap_injector();
    let mut memory = ContextMemory::new();
    memory.append_message(ContextMessage::from_message(Message::assistant("Hello")));

    // When injecting
    let hint = injector.inject(&memory);

    // Then no hint is produced
    assert!(hint.is_none());
}

#[test]
fn should_not_reinject_when_same_hint_already_injected_after_last_user_message() {
    // Given the hint was already injected after the last user message
    let injector = image_gap_injector();
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![image_part("https://example.com/img.png")],
        PromptOrigin::User,
    );
    let hint = injector
        .inject(&memory)
        .expect("first injection should fire");
    memory.append_system_reminder(hint);

    // When injecting again on a later step
    let second = injector.inject(&memory);

    // Then no duplicate hint is produced
    assert!(second.is_none());
}

#[test]
fn should_reinject_when_new_user_message_arrives_after_previous_hint() {
    // Given a previous hint was injected for the first user message
    let injector = image_gap_injector();
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![image_part("https://example.com/one.png")],
        PromptOrigin::User,
    );
    let hint = injector
        .inject(&memory)
        .expect("first injection should fire");
    memory.append_system_reminder(hint);

    // When a new user message with the same modality arrives
    memory.append_user_message(
        vec![image_part("https://example.com/two.png")],
        PromptOrigin::User,
    );

    // Then a fresh hint is produced for the new prompt
    let second = injector
        .inject(&memory)
        .expect("second injection should fire");
    assert!(second.contains("vision-agent"));
}

#[test]
fn should_append_capability_gap_hint_via_manager_when_injector_registered() {
    // Given a manager with the capability gap injector registered and an
    // image-bearing user message in memory
    let mut manager = InjectionManager::new();
    manager.add(Box::new(image_gap_injector()));
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![image_part("https://example.com/img.png")],
        PromptOrigin::User,
    );

    // When the manager injects
    manager.inject(&mut memory);

    // Then an injection-origin reminder naming the vision profile is appended
    let injected: Vec<String> = memory
        .history()
        .iter()
        .filter(|m| m.origin == Some(PromptOrigin::Injection))
        .map(|m| m.message.text_content())
        .collect();
    assert_eq!(injected.len(), 1);
    assert!(injected[0].contains("vision-agent"));
}

/// Compile-time requirement: `CapabilityGapInjector` must satisfy the
/// `Injector` trait's `Send + Sync` bounds when boxed.
#[test]
fn should_be_usable_as_boxed_injector_when_constructed() {
    // Given a capability gap injector boxed as a dyn Injector
    let injector: Box<dyn Injector> = Box::new(image_gap_injector());

    // When it is run against an empty memory
    let memory = ContextMemory::new();
    let hint = injector.inject(&memory);

    // Then it behaves like any injector (no user message -> no hint)
    assert!(hint.is_none());
}

// -----------------------------------------------------------------------
// InjectionManager::add_shared
// -----------------------------------------------------------------------

#[test]
fn should_append_reminder_when_shared_injector_is_added() {
    // Given a manager with a shared injector
    let mut manager = InjectionManager::new();
    manager.add_shared(Arc::new(StaticInjector {
        text: Some("Shared reminder".to_string()),
    }));
    let mut memory = ContextMemory::new();

    // When the manager injects
    manager.inject(&mut memory);

    // Then the shared injector's reminder is appended
    assert!(!memory.history().is_empty());
}

// -----------------------------------------------------------------------
// PermissionModeInjector
// -----------------------------------------------------------------------

#[test]
fn should_inject_auto_mode_reminder_when_mode_is_auto() {
    let pm = Arc::new(PermissionManager::new(PermissionMode::Auto, Vec::new()));
    let injector = PermissionModeInjector::new(pm);
    let memory = ContextMemory::new();

    let result = injector.inject(&memory);
    assert!(result.is_some());
    assert!(result.unwrap().contains("Auto permission mode is active"));
}

#[test]
fn should_not_inject_when_mode_is_not_auto() {
    let pm = Arc::new(PermissionManager::new(PermissionMode::Manual, Vec::new()));
    let injector = PermissionModeInjector::new(pm);
    let memory = ContextMemory::new();

    let result = injector.inject(&memory);
    assert!(result.is_none());
}

#[test]
fn should_not_re_inject_reminder_when_it_was_already_injected() {
    let pm = Arc::new(PermissionManager::new(PermissionMode::Auto, Vec::new()));
    let injector = PermissionModeInjector::new(pm);
    let memory = ContextMemory::new();

    let first = injector.inject(&memory);
    assert!(first.is_some());

    let second = injector.inject(&memory);
    assert!(second.is_none());
}

// -----------------------------------------------------------------------
// PlanModeInjector
// -----------------------------------------------------------------------

#[test]
fn should_inject_full_reminder_when_plan_mode_is_activated() {
    let pm = Arc::new(PlanManager::new());
    pm.enter("Step 1".to_string()).unwrap();
    let injector = PlanModeInjector::new(pm);
    let memory = ContextMemory::new();

    let result = injector.inject(&memory);
    assert!(result.is_some());
    assert!(result.unwrap().contains("Plan mode is active"));
}

#[test]
fn should_inject_exit_reminder_when_plan_mode_is_deactivated() {
    let pm = Arc::new(PlanManager::new());
    pm.enter("Step 1".to_string()).unwrap();
    let injector = PlanModeInjector::new(pm.clone());
    let memory = ContextMemory::new();

    // First call — enter transition.
    let _ = injector.inject(&memory).expect("enter should inject");

    // Exit plan mode.
    pm.exit();

    let result = injector.inject(&memory);
    assert!(result.is_some());
    assert!(result.unwrap().contains("Plan mode is no longer active"));
}

#[test]
fn should_inject_sparse_reminder_when_two_assistant_turns_have_passed() {
    let pm = Arc::new(PlanManager::new());
    pm.enter("Step 1".to_string()).unwrap();
    let injector = PlanModeInjector::new(pm);
    let mut memory = ContextMemory::new();

    // First call — enter transition, injects full reminder. We append it
    // to memory so the injector can find it when counting turns.
    let enter = injector.inject(&memory).expect("enter should inject");
    memory.append_system_reminder(enter);

    // Add 2 assistant turns after the enter reminder.
    memory.append_message(ContextMessage::from_message(Message::assistant("turn 1")));
    memory.append_message(ContextMessage::from_message(Message::assistant("turn 2")));

    let result = injector.inject(&memory);
    assert!(result.is_some());
    assert!(result.unwrap().contains("Plan mode still active"));
}

#[test]
fn should_inject_full_reminder_on_first_activation_when_plan_mode_becomes_active() {
    // This test verifies that a fresh injector (was_active = false) with
    // plan mode active produces the full enter reminder. The memory has
    // 5 assistant turns but no prior plan-mode injection, so the enter
    // transition fires.
    let pm = Arc::new(PlanManager::new());
    pm.enter("Step 1".to_string()).unwrap();
    let injector = PlanModeInjector::new(pm);
    let mut memory = ContextMemory::new();

    memory.append_system_reminder("Plan mode is active. Full reminder.".to_string());
    for i in 1..=5 {
        memory.append_message(ContextMessage::from_message(Message::assistant(format!(
            "turn {i}"
        ))));
    }

    let result = injector.inject(&memory);
    // The injector will first do the enter transition (was_active was false).
    assert!(result.is_some());
    assert!(result.unwrap().contains("Plan mode is active"));
}

#[test]
fn should_inject_full_refresh_after_5_turns_when_already_active() {
    let pm = Arc::new(PlanManager::new());
    pm.enter("Step 1".to_string()).unwrap();
    let injector = PlanModeInjector::new(pm);
    let mut memory = ContextMemory::new();

    // Simulate that the enter transition already happened (was_active = true).
    // We do this by calling inject once (enter transition), then building
    // a memory with the enter reminder + 5 assistant turns.
    let _ = injector.inject(&memory); // Enter transition
    memory.append_system_reminder("Plan mode is active. Full reminder.".to_string());
    for i in 1..=5 {
        memory.append_message(ContextMessage::from_message(Message::assistant(format!(
            "turn {i}"
        ))));
    }

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("Plan mode is active"));
    assert!(!text.contains("still active")); // full, not sparse
}

#[test]
fn should_not_inject_when_plan_mode_is_inactive_and_was_never_active() {
    let pm = Arc::new(PlanManager::new());
    let injector = PlanModeInjector::new(pm);
    let memory = ContextMemory::new();

    let result = injector.inject(&memory);
    assert!(result.is_none());
}

// -----------------------------------------------------------------------
// GoalInjector
// -----------------------------------------------------------------------

#[test]
fn should_inject_active_goal_reminder_when_goal_exists() {
    let gm = Arc::new(GoalManager::new());
    gm.create(
        "Refactor the module".to_string(),
        Some("All tests pass".to_string()),
    )
    .unwrap();
    let injector = GoalInjector::new(gm);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "do the work".into(),
        }],
        PromptOrigin::User,
    );

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("You are working under an active goal"));
    assert!(text.contains("Refactor the module"));
    assert!(text.contains("All tests pass"));
}

#[test]
fn should_not_inject_when_no_goal() {
    let gm = Arc::new(GoalManager::new());
    let injector = GoalInjector::new(gm);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "hello".into(),
        }],
        PromptOrigin::User,
    );

    let result = injector.inject(&memory);
    assert!(result.is_none());
}

#[test]
fn should_not_re_inject_when_already_injected_after_last_user_message() {
    let gm = Arc::new(GoalManager::new());
    gm.create("Some goal".to_string(), None).unwrap();
    let injector = GoalInjector::new(gm);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "do it".into(),
        }],
        PromptOrigin::User,
    );

    // First injection.
    let first = injector.inject(&memory).expect("first should inject");
    memory.append_system_reminder(first);

    // Second call — should skip (already injected after last user message).
    let second = injector.inject(&memory);
    assert!(second.is_none());
}

#[test]
fn should_inject_blocked_note_when_goal_is_blocked() {
    let gm = Arc::new(GoalManager::new());
    gm.create("Blocked goal".to_string(), None).unwrap();
    gm.update(GoalStatus::Blocked);
    let injector = GoalInjector::new(gm);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "hello".into(),
        }],
        PromptOrigin::User,
    );

    let result = injector.inject(&memory);
    assert!(result.is_some());
    assert!(
        result
            .unwrap()
            .contains("There is a goal, currently blocked")
    );
}

#[test]
fn should_inject_paused_note_when_goal_is_paused() {
    let gm = Arc::new(GoalManager::new());
    gm.create("Paused goal".to_string(), None).unwrap();
    gm.update(GoalStatus::Paused);
    let injector = GoalInjector::new(gm);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "hello".into(),
        }],
        PromptOrigin::User,
    );

    let result = injector.inject(&memory);
    assert!(result.is_some());
    assert!(
        result
            .unwrap()
            .contains("There is a goal, currently paused")
    );
}

#[test]
fn should_include_budget_line_when_goal_has_turn_budget() {
    let gm = Arc::new(GoalManager::new());
    gm.create("Test objective".into(), Some("Done when tests pass".into()))
        .unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(10),
        tokens: None,
        wall_clock_ms: None,
    });
    let injector = GoalInjector::new(gm);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "do the work".into(),
        }],
        PromptOrigin::User,
    );

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("Budgets: turns"));
    assert!(text.contains("remaining"));
}

#[test]
fn should_show_nearing_budget_guidance_when_usage_exceeds_75_percent() {
    let gm = Arc::new(GoalManager::new());
    gm.create("Test objective".into(), Some("Done when tests pass".into()))
        .unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(10),
        tokens: None,
        wall_clock_ms: None,
    });
    // Record 8 steps — 8/10 = 80% > 75%.
    for _ in 0..8 {
        let _ = gm.record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
    }
    let injector = GoalInjector::new(gm);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "do the work".into(),
        }],
        PromptOrigin::User,
    );

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("nearing a budget"));
}

#[test]
fn should_show_within_budget_guidance_when_usage_below_75_percent() {
    let gm = Arc::new(GoalManager::new());
    gm.create("Test objective".into(), Some("Done when tests pass".into()))
        .unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(100),
        tokens: None,
        wall_clock_ms: None,
    });
    // Record 1 step — 1/100 = 1% < 75%.
    let _ = gm.record_step_usage(LlmUsage {
        input_tokens: 100,
        output_tokens: 50,
        ..Default::default()
    });
    let injector = GoalInjector::new(gm);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "do the work".into(),
        }],
        PromptOrigin::User,
    );

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("within budget"));
}

// -----------------------------------------------------------------------
// TodoListReminderInjector
// -----------------------------------------------------------------------

#[test]
fn should_inject_reminder_when_todo_list_not_updated_for_10_plus_turns() {
    let todo = Arc::new(TodoList::new());
    let injector = TodoListReminderInjector::new(todo);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "work".into(),
        }],
        PromptOrigin::User,
    );
    for i in 1..=10 {
        memory.append_message(ContextMessage::from_message(Message::assistant(format!(
            "turn {i}"
        ))));
    }

    let result = injector.inject(&memory);
    assert!(result.is_some());
    assert!(
        result
            .unwrap()
            .contains("The TodoList tool has not been updated recently")
    );
}

#[test]
fn should_inject_when_todo_list_was_called_exactly_10_turns_ago() {
    // TodoList write at turn 5 out of 15. Turns since last write = 10
    // (turns 6 through 15). Turns since last reminder = 15 (no prior
    // reminder). Both meet the threshold (>= 10), so it should inject.
    let todo = Arc::new(TodoList::new());
    let injector = TodoListReminderInjector::new(todo);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "work".into(),
        }],
        PromptOrigin::User,
    );
    for i in 1..=15 {
        let mut msg = Message::assistant(format!("turn {i}"));
        if i == 5 {
            msg.tool_calls.push(crate::message::ToolCall {
                id: "call-1".into(),
                call_type: "function".into(),
                function: crate::message::ToolCallFunction {
                    name: "TodoList".into(),
                    arguments: r#"{"todos": [{"title": "task", "status": "pending"}]}"#.into(),
                },
            });
        }
        memory.append_message(ContextMessage::from_message(msg));
    }

    let result = injector.inject(&memory);
    assert!(result.is_some());
}

#[test]
fn should_not_inject_when_todo_list_updated_within_threshold() {
    let todo = Arc::new(TodoList::new());
    let injector = TodoListReminderInjector::new(todo);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "work".into(),
        }],
        PromptOrigin::User,
    );
    // 12 turns, TodoList write at turn 10 (only 2 turns since last write).
    for i in 1..=12 {
        let mut msg = Message::assistant(format!("turn {i}"));
        if i == 10 {
            msg.tool_calls.push(crate::message::ToolCall {
                id: "call-1".into(),
                call_type: "function".into(),
                function: crate::message::ToolCallFunction {
                    name: "TodoList".into(),
                    arguments: r#"{"todos": [{"title": "task", "status": "pending"}]}"#.into(),
                },
            });
        }
        memory.append_message(ContextMessage::from_message(msg));
    }

    let result = injector.inject(&memory);
    assert!(result.is_none());
}

#[test]
fn should_not_inject_when_reminder_was_recently_injected() {
    let todo = Arc::new(TodoList::new());
    let injector = TodoListReminderInjector::new(todo);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "work".into(),
        }],
        PromptOrigin::User,
    );
    // 5 assistant turns, then a reminder, then 5 more.
    for i in 1..=5 {
        memory.append_message(ContextMessage::from_message(Message::assistant(format!(
            "turn {i}"
        ))));
    }
    memory.append_system_reminder(
        "The TodoList tool has not been updated recently. Reminder.".to_string(),
    );
    for i in 6..=10 {
        memory.append_message(ContextMessage::from_message(Message::assistant(format!(
            "turn {i}"
        ))));
    }

    let result = injector.inject(&memory);
    // turns_since_last_reminder = 5 (only 5 assistant turns after the reminder),
    // which is < 10.
    assert!(result.is_none());
}

// -----------------------------------------------------------------------
// ToolsDiffInjector
// -----------------------------------------------------------------------

#[test]
fn should_announce_added_tools_when_tools_are_added_to_turn() {
    let injector = ToolsDiffInjector::new(vec!["Read".into(), "Write".into()], true);
    let memory = ContextMemory::new();

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("<tools_added>"));
    assert!(text.contains("Read"));
    assert!(text.contains("Write"));
}

#[test]
fn should_preserve_current_tool_order_verbatim_when_announcing_added_tools() {
    let injector = ToolsDiffInjector::new(
        vec!["Bash".into(), "Grep".into(), "Read".into(), "Write".into()],
        true,
    );
    let memory = ContextMemory::new();

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    let added_block = text.split("<tools_added>\n").nth(1).unwrap();
    let added_block = &added_block[..added_block.find("</tools_added>").unwrap()];
    assert_eq!(added_block, "Bash\nGrep\nRead\nWrite\n");
}

#[test]
fn should_announce_removed_tools_when_tools_are_removed_from_turn() {
    // History has an announcement of Read and Write, current has only Read.
    let injector = ToolsDiffInjector::new(vec!["Read".into()], true);
    let mut memory = ContextMemory::new();
    memory.append_system_reminder("<tools_added>\nRead\nWrite\n</tools_added>".to_string());

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("<tools_removed>"));
    assert!(text.contains("Write"));
}

#[test]
fn should_not_announce_when_no_changes() {
    // History has an announcement of Read and Write, current has both.
    let injector = ToolsDiffInjector::new(vec!["Read".into(), "Write".into()], true);
    let mut memory = ContextMemory::new();
    memory.append_system_reminder("<tools_added>\nRead\nWrite\n</tools_added>".to_string());

    let result = injector.inject(&memory);
    assert!(result.is_none());
}

#[test]
fn should_fold_previous_announcements_when_history_contains_announcements() {
    // History: first adds Read, then removes Read, then adds Write.
    // Current: Read, Write.
    // After folding: announced = {Write} (Read was added then removed).
    // Current = {Read, Write}.
    // Added: Read (not in announced). Removed: none.
    let injector = ToolsDiffInjector::new(vec!["Read".into(), "Write".into()], true);
    let mut memory = ContextMemory::new();
    memory.append_system_reminder("<tools_added>\nRead\n</tools_added>".to_string());
    memory.append_system_reminder("<tools_removed>\nRead\n</tools_removed>".to_string());
    memory.append_system_reminder("<tools_added>\nWrite\n</tools_added>".to_string());

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("<tools_added>"));
    assert!(text.contains("Read"));
    assert!(!text.contains("<tools_removed>"));
}

#[test]
fn should_fold_tools_added_when_origin_is_system_trigger() {
    // A SystemTrigger-origin message announces tool_a and tool_b as added.
    // Current enabled tools are tool_a and tool_c.
    // After folding: announced = {tool_a, tool_b}, current = {tool_a, tool_c}.
    // Added: tool_c (in current, not announced). Removed: tool_b (announced,
    // not current).
    let injector = ToolsDiffInjector::new(vec!["tool_a".into(), "tool_c".into()], true);
    let mut memory = ContextMemory::new();

    // A real user message so the memory is not empty.
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "hello".into(),
        }],
        PromptOrigin::User,
    );

    // Append a SystemTrigger-origin message with tool announcements.
    let msg = ContextMessage {
            message: Message {
                role: Role::User,
                name: None,
                content: vec![ContentPart::Text {
                    text: "<system-reminder>\n<tools_added>\ntool_a\ntool_b\n</tools_added>\n</system-reminder>"
                        .to_string(),
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            },
            origin: Some(PromptOrigin::SystemTrigger),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        };
    memory.append_message(msg);

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    // tool_c is in current but not in announced → added.
    assert!(text.contains("<tools_added>"));
    assert!(text.contains("tool_c"));
    // tool_b was announced but is not in current → removed.
    assert!(text.contains("<tools_removed>"));
    assert!(text.contains("tool_b"));
    // tool_a was announced and is in current → no announcement.
    assert!(!text.contains("tool_a\n"));
}

// Given progressive disclosure is disabled, when a tools_added announcement is injected,
// then the select_tools instruction tail is omitted
#[test]
fn should_omit_select_tools_tail_when_disclosure_is_disabled() {
    let injector = ToolsDiffInjector::new(vec!["Read".into()], false);
    let memory = ContextMemory::new();

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    // The added-tools list is still announced…
    assert!(text.contains("<tools_added>"));
    assert!(text.contains("Read"));
    // …but the select_tools guidance tail is not: with disclosure disabled
    // select_tools is not configured for the agent, so pointing the model at
    // it would contradict the tool registry state (tool-calling ADR-3).
    assert!(!text.contains("Use the select_tools tool"));
}

// Given progressive disclosure is enabled, when a tools_added announcement is injected,
// then the select_tools instruction tail is kept verbatim
#[test]
fn should_keep_select_tools_tail_when_disclosure_is_enabled() {
    let injector = ToolsDiffInjector::new(vec!["Read".into()], true);
    let memory = ContextMemory::new();

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("<tools_added>"));
    assert!(text.contains(
        "Use the select_tools tool with exact names to load full tool definitions before calling them. Names listed as removed are no longer loadable — do not select them. Fold all announcements in this conversation in order to get the current list."
    ));
}

// Given a tools snapshot carrying an MCP glob pattern alongside real tool
// names, when the first announcement is injected, then the pattern is not
// announced as a tool (it is a whitelist directive, not a callable name)
#[test]
fn should_not_announce_glob_patterns_as_tools() {
    let injector =
        ToolsDiffInjector::new(vec!["Read".into(), "Write".into(), "mcp__*".into()], false);
    let memory = ContextMemory::new();

    let result = injector.inject(&memory);
    assert!(result.is_some());
    let text = result.unwrap();
    assert!(text.contains("Read"));
    assert!(text.contains("Write"));
    assert!(!text.contains("mcp__*"));
}
