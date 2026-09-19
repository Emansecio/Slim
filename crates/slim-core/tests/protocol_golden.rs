use slim_core::{EventKind, OperatingMode, ProfileCatalog, ProfileId, SessionEvent};

#[test]
fn event_sequence_is_monotonic_and_covers_the_initial_contract() {
    let events = vec![
        SessionEvent::new(
            1,
            EventKind::SessionStarted {
                session_id: "s1".into(),
            },
        ),
        SessionEvent::new(
            2,
            EventKind::ModeChanged {
                mode: OperatingMode::ReadOnly,
            },
        ),
        SessionEvent::new(
            3,
            EventKind::AssistantTextDelta {
                text: "hello".into(),
            },
        ),
        SessionEvent::new(
            4,
            EventKind::ToolStarted {
                batch_id: "batch-1".into(),
                call_id: "call-1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        ),
        SessionEvent::new(
            5,
            EventKind::ProviderToolCall {
                id: "provider-call-1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        ),
        SessionEvent::new(
            6,
            EventKind::ToolFinished {
                batch_id: "batch-1".into(),
                call_id: "call-1".into(),
                name: "read".into(),
                success: true,
                duration_ms: 1,
            },
        ),
        SessionEvent::new(7, EventKind::CompactionCompleted),
        SessionEvent::new(
            8,
            EventKind::ApprovalRequired {
                request_id: "approval-1".into(),
                summary: "apply changes".into(),
                persisted: true,
            },
        ),
        SessionEvent::new(
            9,
            EventKind::InputRequired {
                request_id: "input-1".into(),
                prompt: "choose target".into(),
                options: vec!["core".into(), "tui".into()],
                persisted: true,
            },
        ),
        SessionEvent::new(
            10,
            EventKind::SubagentActivity {
                message: "child".into(),
            },
        ),
        SessionEvent::new(
            11,
            EventKind::TerminalError {
                message: "done".into(),
            },
        ),
    ];

    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    assert_eq!(events.first().map(|event| event.seq), Some(1));
    assert_eq!(events.last().map(|event| event.seq), Some(11));
    assert_eq!(OperatingMode::default(), OperatingMode::Auto);
    assert_eq!(OperatingMode::Auto.next(), OperatingMode::ReadOnly);
    assert_eq!(OperatingMode::ReadOnly.next(), OperatingMode::Plan);
    assert_eq!(OperatingMode::Plan.next(), OperatingMode::Auto);
    assert!(OperatingMode::Auto.allows_mutation());
    assert!(!OperatingMode::ReadOnly.allows_mutation());
    assert!(!OperatingMode::Plan.allows_mutation());

    let profiles = ProfileCatalog;
    assert_eq!(profiles.get(ProfileId::Deep).effort, "high");
}

#[test]
fn legacy_interaction_events_deserialize_without_inventing_a_route() {
    let approval: EventKind =
        serde_json::from_str(r#"{"type":"ApprovalRequired"}"#).expect("legacy approval");
    let input: EventKind =
        serde_json::from_str(r#"{"type":"InputRequired"}"#).expect("legacy input");

    assert_eq!(
        approval,
        EventKind::ApprovalRequired {
            request_id: String::new(),
            summary: String::new(),
            persisted: false,
        }
    );
    assert_eq!(
        input,
        EventKind::InputRequired {
            request_id: String::new(),
            prompt: String::new(),
            options: Vec::new(),
            persisted: false,
        }
    );
}
