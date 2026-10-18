use mirri_proto::DomainEvent;

#[test]
fn should_include_type_tag_when_serializing_domain_event() {
    let event = DomainEvent::TurnStarted { turn_id: 1 };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "turn_started");
    assert_eq!(json["turn_id"], 1);
}

#[test]
fn should_deserialize_correct_variant_when_parsing_json() {
    let json = r#"{"type":"goal_started","objective":"test"}"#;
    let event: DomainEvent = serde_json::from_str(json).unwrap();
    assert!(matches!(event, DomainEvent::GoalStarted { objective } if objective == "test"));
}

#[test]
fn should_preserve_all_fields_when_roundtripping_domain_event() {
    let event = DomainEvent::MessageAdded {
        role: "user".to_string(),
        content_preview: "hello".to_string(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deserialized: DomainEvent = serde_json::from_str(&json).unwrap();
    assert!(
        matches!(deserialized, DomainEvent::MessageAdded { role, content_preview } if role == "user" && content_preview == "hello")
    );
}

#[test]
fn should_deserialize_to_unknown_when_type_tag_is_unrecognized() {
    let json = r#"{"type":"some_future_event","data":"x"}"#;
    let event: DomainEvent = serde_json::from_str(json).unwrap();
    assert!(matches!(event, DomainEvent::Unknown));
}

#[test]
fn should_serialize_and_deserialize_when_event_variant_has_no_fields() {
    let events = vec![
        DomainEvent::GoalCleared {},
        DomainEvent::ContextCleared {},
        DomainEvent::GoalCompleted {},
        DomainEvent::GoalResumed {},
        DomainEvent::PlanModeExited {},
    ];
    for event in &events {
        let json = serde_json::to_string(event).unwrap();
        let _: DomainEvent = serde_json::from_str(&json).unwrap();
    }
}
